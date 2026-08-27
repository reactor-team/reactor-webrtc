//! Integration tests for PeerConnection::get_stats.
//!
//! Establishes a loopback connection (same process, two PeerConnections) and
//! verifies that get_stats returns a non-empty report with at least one ICE
//! candidate pair. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_PREBUILT_URL=... cargo test --test stats -- --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        IceCandidate, IceCandidatePairState, PeerConnection, PeerConnectionFactory,
        PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
    };

    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
    }

    impl Default for Peer {
        fn default() -> Self {
            Self {
                ice: Mutex::new(VecDeque::new()),
                connected: AtomicBool::new(false),
            }
        }
    }

    fn make_peer(
        factory: &PeerConnectionFactory,
        cfg: &RtcConfiguration,
    ) -> (PeerConnection, Arc<Peer>) {
        let shared = Arc::new(Peer::default());
        let obs = PeerConnectionObserver::new()
            .on_ice_candidate({
                let s = shared.clone();
                move |c| s.ice.lock().unwrap().push_back(c)
            })
            .on_connection_state_change({
                let s = shared.clone();
                move |st| {
                    if st == PeerConnectionState::Connected {
                        s.connected.store(true, Ordering::SeqCst);
                    }
                }
            });
        let pc = factory
            .create_peer_connection(cfg, obs)
            .expect("peer connection");
        (pc, shared)
    }

    fn trickle(from: &Peer, to: &PeerConnection) {
        while let Some(c) = from.ice.lock().unwrap().pop_front() {
            let _ = to.add_ice_candidate(&c);
        }
    }

    fn negotiate(pc1: &PeerConnection, pc2: &PeerConnection) {
        let offer = pc1.create_offer().expect("offer");
        pc1.set_local_description(&offer).expect("local");
        pc2.set_remote_description(&offer).expect("remote");
        let answer = pc2.create_answer().expect("answer");
        pc2.set_local_description(&answer).expect("local");
        pc1.set_remote_description(&answer).expect("remote");
    }

    fn wait_for(
        s1: &Peer,
        s2: &Peer,
        pc1: &PeerConnection,
        pc2: &PeerConnection,
        done: impl Fn() -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            trickle(s1, pc2);
            trickle(s2, pc1);
            if done() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn get_stats_connected_peer() {
        let factory = PeerConnectionFactory::builder().build().expect("factory");
        let cfg = RtcConfiguration::default();

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        // A data channel forces SCTP negotiation, which gives the DTLS transport
        // something to connect over — without it PeerConnectionState::Connected
        // is never reached and the test times out.
        let _dc = pc1.create_data_channel("stats-probe").expect("dc");

        negotiate(&pc1, &pc2);

        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst)
        });
        assert!(ok, "timed out waiting for connection");

        // The stats snapshot may lag the connection event by a tick or two;
        // poll until a Succeeded pair appears (or we time out).
        let report = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let r = pc1.get_stats().expect("get_stats");
                if r.candidate_pairs
                    .iter()
                    .any(|p| p.state == IceCandidatePairState::Succeeded)
                {
                    break r;
                }
                if Instant::now() >= deadline {
                    break r;
                }
                thread::sleep(Duration::from_millis(50));
            }
        };

        // A connected loopback peer must have at least one succeeded
        // candidate pair.
        assert!(
            !report.candidate_pairs.is_empty(),
            "expected candidate pair stats, got none"
        );
        let succeeded = report
            .candidate_pairs
            .iter()
            .any(|p| p.state == IceCandidatePairState::Succeeded);
        assert!(succeeded, "no succeeded candidate pair found");

        println!(
            "get_stats_connected_peer ✅\n  \
             inbound_rtp:     {}\n  \
             outbound_rtp:    {}\n  \
             candidate_pairs: {}",
            report.inbound_rtp.len(),
            report.outbound_rtp.len(),
            report.candidate_pairs.len(),
        );
        for p in &report.candidate_pairs {
            println!(
                "  pair  state={:?}  rtt={:.3}ms  priority={}",
                p.state,
                p.current_round_trip_time_s * 1000.0,
                p.priority,
            );
        }
    }

    #[test]
    fn get_stats_returns_immediately_when_not_connected() {
        let factory = PeerConnectionFactory::builder().build().expect("factory");
        let cfg = RtcConfiguration::default();
        let obs = PeerConnectionObserver::new();
        let pc = factory.create_peer_connection(&cfg, obs).expect("pc");

        // No signaling, no ICE — should still return (empty report).
        let report = pc.get_stats().expect("get_stats on idle pc");
        // Candidate pairs require an ICE negotiation; none expected here.
        assert!(
            report.candidate_pairs.is_empty(),
            "unexpected candidate pairs on idle pc"
        );
        println!("get_stats_returns_immediately_when_not_connected ✅");
    }
}
