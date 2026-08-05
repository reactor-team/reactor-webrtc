//! Peer-connection configuration (mirrors the PoC's `RtcConfiguration`).

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

use reactor_webrtc_sys::{ReactorIceServer, ReactorRtcConfig};

use crate::{Error, Result};

/// A single ICE (STUN/TURN) server.
///
/// All URLs in one entry share `username` and `password`. libwebrtc rejects the
/// whole configuration when a `turn:` or `turns:` URL carries an empty username
/// or password, so credentialed TURN servers need both fields set.
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

/// How m-sections are bundled onto a single transport.
///
/// [`MaxBundle`] is the right choice for real-time streaming: all tracks share
/// one DTLS+SRTP association, which halves the ICE pairs that need to succeed
/// and reduces per-packet overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BundlePolicy {
    /// Balanced — libwebrtc default.
    #[default]
    Balanced,
    /// All m-sections share one transport (recommended for streaming).
    MaxBundle,
    /// One transport per m-section (maximises compatibility with legacy stacks).
    MaxCompat,
}

impl BundlePolicy {
    fn to_wire(self) -> c_int {
        match self {
            Self::Balanced => 0,
            Self::MaxBundle => 1,
            Self::MaxCompat => 2,
        }
    }
}

/// Whether libwebrtc may gather TCP ICE candidates.
///
/// TCP is disabled by default because it adds latency. Enable it only when UDP
/// is blocked (corporate firewalls, symmetric NATs without TURN).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TcpCandidatePolicy {
    /// TCP ICE candidates are not gathered (libwebrtc default).
    #[default]
    Disabled,
    /// TCP ICE candidates are gathered alongside UDP.
    Enabled,
}

impl TcpCandidatePolicy {
    fn to_wire(self) -> c_int {
        match self {
            Self::Disabled => 0,
            Self::Enabled => 1,
        }
    }
}

impl ContinualGatheringPolicy {
    /// The wire value understood by the glue. Explicit, so reordering the
    /// variants cannot silently change the meaning on the native side.
    fn to_wire(self) -> c_int {
        match self {
            Self::GatherOnce => 0,
            Self::GatherContinually => 1,
        }
    }
}

/// Which ICE candidate types are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceTransportsType {
    All,
    Relay,
    NoHost,
    None,
}

impl IceTransportsType {
    /// The wire value understood by the glue. Explicit, so reordering the
    /// variants cannot silently change the meaning on the native side.
    fn to_wire(self) -> c_int {
        match self {
            Self::All => 0,
            Self::Relay => 1,
            Self::NoHost => 2,
            Self::None => 3,
        }
    }
}

/// Configuration passed to [`crate::PeerConnectionFactory::create_peer_connection`].
#[derive(Debug, Clone)]
pub struct RtcConfiguration {
    pub ice_servers: Vec<IceServer>,
    pub continual_gathering_policy: ContinualGatheringPolicy,
    pub ice_transport_type: IceTransportsType,
    /// Lower bound of the UDP port range ICE may use. `None` leaves the
    /// libwebrtc default (OS-assigned ephemeral ports).
    pub min_port: Option<u16>,
    /// Upper bound of the UDP port range ICE may use. `None` leaves the
    /// libwebrtc default (OS-assigned ephemeral ports).
    pub max_port: Option<u16>,
    /// How m-sections are bundled onto a single transport.
    /// [`BundlePolicy::MaxBundle`] is recommended for streaming.
    pub bundle_policy: BundlePolicy,
    /// How long ICE waits for a response before declaring the path failed.
    ///
    /// libwebrtc's default is very conservative (~30 s in practice). For
    /// real-time streaming, `Some(2000)` to `Some(4000)` detects failures fast
    /// enough to trigger reconnect before users notice a freeze.
    pub ice_connection_receiving_timeout_ms: Option<i32>,
    /// Minimum interval between ICE connectivity checks on a good path (ms).
    ///
    /// `None` keeps the libwebrtc default (~500 ms). Lowering this (e.g.
    /// `Some(250)`) makes keepalives more frequent, trading bandwidth for
    /// faster detection of path changes.
    pub ice_check_min_interval_ms: Option<i32>,
    /// Whether libwebrtc gathers TCP ICE candidates.
    /// Disabled by default; enable only when UDP is firewalled.
    pub tcp_candidate_policy: TcpCandidatePolicy,
}

impl Default for RtcConfiguration {
    fn default() -> Self {
        Self {
            ice_servers: Vec::new(),
            // GatherOnce is libwebrtc's own default. It lets ICE gathering
            // reach the Complete state, which a caller that waits for
            // gathering to finish depends on. GatherContinually keeps
            // gathering for the life of the connection and never reports
            // Complete, so it must be an explicit choice.
            continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
            ice_transport_type: IceTransportsType::All,
            min_port: None,
            max_port: None,
            bundle_policy: BundlePolicy::default(),
            ice_connection_receiving_timeout_ms: None,
            ice_check_min_interval_ms: None,
            tcp_candidate_policy: TcpCandidatePolicy::default(),
        }
    }
}

impl RtcConfiguration {
    /// Marshal into the C layout the glue reads, keeping every string alive for
    /// as long as the returned [`NativeConfig`] lives.
    pub(crate) fn to_native(&self) -> Result<NativeConfig> {
        NativeConfig::new(self)
    }
}

/// Owns the C strings and pointer arrays behind a [`ReactorRtcConfig`].
///
/// The glue borrows every pointer for the duration of one
/// `reactor_webrtc_peer_connection_create` call, so this value must outlive the
/// call. `config()` hands out a struct whose pointers alias `self`, which is why
/// it borrows `self` for the returned struct's lifetime.
pub(crate) struct NativeConfig {
    /// Keeps the URL strings alive. Every allocation lives on the heap, so the
    /// pointers in `servers` stay valid when this value moves.
    _urls: Vec<Vec<CString>>,
    /// Keeps the username and password strings alive, one pair per server.
    _credentials: Vec<(CString, CString)>,
    /// Keeps the per-server URL pointer arrays alive.
    _url_ptrs: Vec<Vec<*const c_char>>,
    servers: Vec<ReactorIceServer>,
    ice_transport_type: c_int,
    continual_gathering_policy: c_int,
    min_port: c_int,
    max_port: c_int,
    bundle_policy: c_int,
    ice_connection_receiving_timeout_ms: c_int,
    ice_check_min_interval_ms: c_int,
    tcp_candidate_policy: c_int,
}

impl NativeConfig {
    fn new(config: &RtcConfiguration) -> Result<Self> {
        let nul = |field: &str| Error::Webrtc(format!("ICE server {field} contains a NUL byte"));

        let mut urls: Vec<Vec<CString>> = Vec::with_capacity(config.ice_servers.len());
        let mut credentials: Vec<(CString, CString)> = Vec::with_capacity(config.ice_servers.len());
        for server in &config.ice_servers {
            let mut entry = Vec::with_capacity(server.urls.len());
            for url in &server.urls {
                entry.push(CString::new(url.as_str()).map_err(|_| nul("URL"))?);
            }
            urls.push(entry);
            credentials.push((
                CString::new(server.username.as_str()).map_err(|_| nul("username"))?,
                CString::new(server.password.as_str()).map_err(|_| nul("password"))?,
            ));
        }

        let url_ptrs: Vec<Vec<*const c_char>> = urls
            .iter()
            .map(|entry| entry.iter().map(|url| url.as_ptr()).collect())
            .collect();

        let servers = url_ptrs
            .iter()
            .zip(&credentials)
            .map(|(ptrs, (username, password))| ReactorIceServer {
                urls: ptrs.as_ptr(),
                urls_len: ptrs.len(),
                username: username.as_ptr(),
                password: password.as_ptr(),
            })
            .collect();

        let (min_port, max_port) = match (config.min_port, config.max_port) {
            (None, None) => (0, 0),
            (Some(lo), Some(hi)) if lo <= hi => (lo as c_int, hi as c_int),
            (Some(lo), Some(hi)) => {
                return Err(Error::Webrtc(format!(
                    "min_port ({lo}) must be <= max_port ({hi})"
                )));
            }
            _ => {
                return Err(Error::Webrtc(
                    "port range requires both min_port and max_port".into(),
                ));
            }
        };

        Ok(Self {
            _urls: urls,
            _credentials: credentials,
            _url_ptrs: url_ptrs,
            servers,
            ice_transport_type: config.ice_transport_type.to_wire(),
            continual_gathering_policy: config.continual_gathering_policy.to_wire(),
            min_port,
            max_port,
            bundle_policy: config.bundle_policy.to_wire(),
            ice_connection_receiving_timeout_ms: config
                .ice_connection_receiving_timeout_ms
                .unwrap_or(-1),
            ice_check_min_interval_ms: config.ice_check_min_interval_ms.unwrap_or(-1),
            tcp_candidate_policy: config.tcp_candidate_policy.to_wire(),
        })
    }

    /// The struct to pass to the glue. Valid while `self` is alive.
    pub(crate) fn config(&self) -> ReactorRtcConfig {
        ReactorRtcConfig {
            servers: self.servers.as_ptr(),
            servers_len: self.servers.len(),
            ice_transport_type: self.ice_transport_type,
            continual_gathering_policy: self.continual_gathering_policy,
            min_port: self.min_port,
            max_port: self.max_port,
            bundle_policy: self.bundle_policy,
            ice_connection_receiving_timeout_ms: self.ice_connection_receiving_timeout_ms,
            ice_check_min_interval_ms: self.ice_check_min_interval_ms,
            tcp_candidate_policy: self.tcp_candidate_policy,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    /// Read one marshalled server back through the raw pointers the glue reads.
    fn read_server(native: &NativeConfig, index: usize) -> (Vec<String>, String, String) {
        let config = native.config();
        assert!(index < config.servers_len, "server index out of range");
        // SAFETY: `native` owns every allocation these pointers reference, and
        // it outlives this call.
        unsafe {
            let server = &*config.servers.add(index);
            let cstr = |p: *const c_char| CStr::from_ptr(p).to_string_lossy().into_owned();
            let urls = (0..server.urls_len)
                .map(|i| cstr(*server.urls.add(i)))
                .collect();
            (urls, cstr(server.username), cstr(server.password))
        }
    }

    #[test]
    fn keeps_credentials_with_their_own_server() {
        let config = RtcConfiguration {
            ice_servers: vec![
                IceServer {
                    urls: vec!["stun:stun.example.com:19302".into()],
                    ..Default::default()
                },
                IceServer {
                    urls: vec!["turn:a.example.com:3478".into()],
                    username: "alice".into(),
                    password: "secret-a".into(),
                },
                IceServer {
                    urls: vec![
                        "turn:b.example.com:3478".into(),
                        "turns:b.example.com:443".into(),
                    ],
                    username: "bob".into(),
                    password: "secret-b".into(),
                },
            ],
            ..Default::default()
        };
        let native = config.to_native().expect("marshal");

        // One entry per IceServer: URLs never collapse into a single entry, so
        // each server keeps the credentials it was given.
        assert_eq!(native.config().servers_len, 3);
        assert_eq!(
            read_server(&native, 0),
            (
                vec!["stun:stun.example.com:19302".to_string()],
                String::new(),
                String::new()
            )
        );
        assert_eq!(
            read_server(&native, 1),
            (
                vec!["turn:a.example.com:3478".to_string()],
                "alice".to_string(),
                "secret-a".to_string()
            )
        );
        assert_eq!(
            read_server(&native, 2),
            (
                vec![
                    "turn:b.example.com:3478".to_string(),
                    "turns:b.example.com:443".to_string()
                ],
                "bob".to_string(),
                "secret-b".to_string()
            )
        );
    }

    #[test]
    fn passes_urls_and_credentials_verbatim() {
        let config = RtcConfiguration {
            ice_servers: vec![IceServer {
                urls: vec!["turns:turn.example.com:443?transport=tcp".into()],
                username: "1753790400:user".into(),
                // A credential may hold any byte, including characters that
                // look like JSON punctuation or an ICE-server URL.
                password: r#"p"a\ss:turn:x"#.into(),
            }],
            ..Default::default()
        };
        let native = config.to_native().expect("marshal");

        assert_eq!(native.config().servers_len, 1);
        assert_eq!(
            read_server(&native, 0),
            (
                vec!["turns:turn.example.com:443?transport=tcp".to_string()],
                "1753790400:user".to_string(),
                r#"p"a\ss:turn:x"#.to_string()
            )
        );
    }

    #[test]
    fn sends_policies_as_explicit_wire_values() {
        let default = RtcConfiguration::default().to_native().expect("marshal");
        assert_eq!(default.config().ice_transport_type, 0);
        assert_eq!(default.config().continual_gathering_policy, 0);

        let relay_continually = RtcConfiguration {
            ice_transport_type: IceTransportsType::Relay,
            continual_gathering_policy: ContinualGatheringPolicy::GatherContinually,
            ..Default::default()
        }
        .to_native()
        .expect("marshal");
        assert_eq!(relay_continually.config().ice_transport_type, 1);
        assert_eq!(relay_continually.config().continual_gathering_policy, 1);

        let no_host = RtcConfiguration {
            ice_transport_type: IceTransportsType::NoHost,
            ..Default::default()
        }
        .to_native()
        .expect("marshal");
        assert_eq!(no_host.config().ice_transport_type, 2);

        let none = RtcConfiguration {
            ice_transport_type: IceTransportsType::None,
            ..Default::default()
        }
        .to_native()
        .expect("marshal");
        assert_eq!(none.config().ice_transport_type, 3);
    }

    #[test]
    fn passes_port_range_to_native() {
        let default = RtcConfiguration::default().to_native().expect("marshal");
        assert_eq!(default.config().min_port, 0);
        assert_eq!(default.config().max_port, 0);

        let ranged = RtcConfiguration {
            min_port: Some(10000),
            max_port: Some(10100),
            ..Default::default()
        }
        .to_native()
        .expect("marshal");
        assert_eq!(ranged.config().min_port, 10000);
        assert_eq!(ranged.config().max_port, 10100);

        // u16::MAX round-trips without truncation.
        let max_u16 = RtcConfiguration {
            min_port: Some(u16::MAX),
            max_port: Some(u16::MAX),
            ..Default::default()
        }
        .to_native()
        .expect("marshal");
        assert_eq!(max_u16.config().min_port, u16::MAX as c_int);
        assert_eq!(max_u16.config().max_port, u16::MAX as c_int);
    }

    #[test]
    fn rejects_half_specified_or_inverted_port_range() {
        let min_only = RtcConfiguration {
            min_port: Some(49152),
            ..Default::default()
        };
        let err = min_only
            .to_native()
            .err()
            .expect("min_only must be rejected")
            .to_string();
        assert!(err.contains("both min_port and max_port"), "got: {err}");

        let max_only = RtcConfiguration {
            max_port: Some(49152),
            ..Default::default()
        };
        let err = max_only
            .to_native()
            .err()
            .expect("max_only must be rejected")
            .to_string();
        assert!(err.contains("both min_port and max_port"), "got: {err}");

        let inverted = RtcConfiguration {
            min_port: Some(20000),
            max_port: Some(10000),
            ..Default::default()
        };
        let err = inverted
            .to_native()
            .err()
            .expect("inverted range must be rejected")
            .to_string();
        assert!(
            err.contains("min_port") && err.contains("max_port"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_a_nul_byte() {
        for server in [
            IceServer {
                urls: vec!["turn:a.example.com:3478\0".into()],
                username: "alice".into(),
                password: "secret".into(),
            },
            IceServer {
                urls: vec!["turn:a.example.com:3478".into()],
                username: "al\0ice".into(),
                password: "secret".into(),
            },
            IceServer {
                urls: vec!["turn:a.example.com:3478".into()],
                username: "alice".into(),
                password: "sec\0ret".into(),
            },
        ] {
            let config = RtcConfiguration {
                ice_servers: vec![server],
                ..Default::default()
            };
            let Err(err) = config.to_native() else {
                panic!("a NUL byte must be rejected");
            };
            assert!(
                err.to_string().contains("NUL byte"),
                "unexpected error: {err}"
            );
        }
    }
}
