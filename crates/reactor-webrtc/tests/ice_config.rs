//! Integration tests for the ICE configuration that reaches libwebrtc.
//!
//! These assert on native acceptance: every case calls
//! `create_peer_connection`, because a configuration can round-trip through the
//! Rust structs perfectly and still be rejected by libwebrtc. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_PREBUILT_URL=... cargo test --test ice_config -- --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use reactor_webrtc::{
        ContinualGatheringPolicy, IceServer, IceTransportsType, PeerConnectionFactory,
        PeerConnectionObserver, RtcConfiguration,
    };

    fn stun() -> IceServer {
        IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            ..Default::default()
        }
    }

    fn credentialed(url: &str) -> IceServer {
        IceServer {
            urls: vec![url.into()],
            username: "alice".into(),
            password: "secret".into(),
        }
    }

    fn with_servers(ice_servers: Vec<IceServer>) -> RtcConfiguration {
        RtcConfiguration {
            ice_servers,
            ..Default::default()
        }
    }

    /// Every case runs on one factory. libwebrtc starts process-global threads
    /// on factory creation and joins them on destruction, so repeated factory
    /// churn in a single process is unsafe.
    #[test]
    fn libwebrtc_accepts_every_credentialed_ice_configuration() {
        let factory = PeerConnectionFactory::new().expect("factory");

        let cases: Vec<(&str, RtcConfiguration)> = vec![
            ("no ice servers", RtcConfiguration::default()),
            ("stun only", with_servers(vec![stun()])),
            (
                "turn with credentials",
                with_servers(vec![credentialed("turn:turn.example.com:3478")]),
            ),
            (
                "turns with credentials",
                with_servers(vec![credentialed("turns:turn.example.com:443")]),
            ),
            (
                "stun plus credentialed turn and turns",
                with_servers(vec![
                    stun(),
                    credentialed("turn:turn.example.com:3478?transport=udp"),
                    credentialed("turns:turn.example.com:443?transport=tcp"),
                ]),
            ),
            (
                "two turn servers with distinct credentials",
                with_servers(vec![
                    IceServer {
                        urls: vec!["turn:a.example.com:3478".into()],
                        username: "alice".into(),
                        password: "secret-a".into(),
                    },
                    IceServer {
                        urls: vec!["turn:b.example.com:3478".into()],
                        username: "bob".into(),
                        password: "secret-b".into(),
                    },
                ]),
            ),
            (
                "relay-only policy, gather continually",
                RtcConfiguration {
                    ice_servers: vec![credentialed("turn:turn.example.com:3478")],
                    ice_transport_type: IceTransportsType::Relay,
                    continual_gathering_policy: ContinualGatheringPolicy::GatherContinually,
                    ..Default::default()
                },
            ),
            (
                "explicit port range",
                RtcConfiguration {
                    min_port: Some(10000),
                    max_port: Some(10100),
                    ..Default::default()
                },
            ),
        ];

        for (label, config) in &cases {
            let pc = factory.create_peer_connection(config, PeerConnectionObserver::new());
            assert!(pc.is_ok(), "{label}: {}", pc.err().unwrap());
            println!("{label} ✅");
        }
    }

    #[test]
    fn libwebrtc_rejects_turn_without_credentials_and_reports_why() {
        let factory = PeerConnectionFactory::new().expect("factory");
        let config = with_servers(vec![IceServer {
            urls: vec!["turn:turn.example.com:3478".into()],
            ..Default::default()
        }]);

        let err = factory
            .create_peer_connection(&config, PeerConnectionObserver::new())
            .err()
            .expect("libwebrtc must reject a TURN server without credentials");

        // The failure carries libwebrtc's own reason, so a caller can tell an
        // empty credential apart from any other rejection.
        let message = err.to_string();
        assert!(
            message.contains("username or password"),
            "expected the empty-credential reason, got: {message}"
        );
        println!("turn without credentials rejected ✅ — {message}");
    }

    #[test]
    fn rejects_half_specified_and_inverted_port_range() {
        for (label, config) in [
            (
                "min_port only",
                RtcConfiguration {
                    min_port: Some(10000),
                    ..Default::default()
                },
            ),
            (
                "max_port only",
                RtcConfiguration {
                    max_port: Some(10100),
                    ..Default::default()
                },
            ),
            (
                "inverted range",
                RtcConfiguration {
                    min_port: Some(10100),
                    max_port: Some(10000),
                    ..Default::default()
                },
            ),
        ] {
            let result = PeerConnectionFactory::new()
                .expect("factory")
                .create_peer_connection(&config, PeerConnectionObserver::new());
            assert!(result.is_err(), "{label}: expected error but got Ok");
            println!("{label} correctly rejected ✅ — {}", result.err().unwrap());
        }
    }
}
