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
        IceCandidate, IceCandidatePairState, IceCandidateType, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RelayProtocol,
        RtcConfiguration,
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
                "  pair  state={:?}  nominated={}  rtt={:.3}ms  priority={}  \
                 candidate={:?}  relay={:?}  bytes={}↑/{}↓  avail={:.0}↑/{:.0}↓ bps",
                p.state,
                p.nominated,
                p.current_round_trip_time_s * 1000.0,
                p.priority,
                p.local_candidate_type,
                p.local_relay_protocol,
                p.bytes_sent,
                p.bytes_received,
                p.available_outgoing_bitrate_bps,
                p.available_incoming_bitrate_bps,
            );
        }
    }

    /// The fields REA-6019 added, on the pair ICE actually chose.
    ///
    /// Asserted on the *nominated* pair rather than on "any pair": a loopback
    /// connection gathers several, and the ones ICE did not select have no byte
    /// counters and no bitrate estimate. Asserting across all of them would pass
    /// on a pair that carried nothing.
    #[test]
    fn the_nominated_pair_reports_its_candidate_type_and_counters() {
        let factory = PeerConnectionFactory::builder().build().expect("factory");
        let cfg = RtcConfiguration::default();

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let _dc = pc1.create_data_channel("stats-probe").expect("dc");

        negotiate(&pc1, &pc2);
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst)
        });
        assert!(ok, "timed out waiting for connection");

        // Nomination and the first byte counters land a tick or two after the
        // connection event, so this polls for the nominated pair rather than
        // reading one snapshot and hoping.
        let deadline = Instant::now() + Duration::from_secs(5);
        let pair = loop {
            let report = pc1.get_stats().expect("get_stats");
            if let Some(p) = report
                .candidate_pairs
                .into_iter()
                .find(|p| p.nominated && p.bytes_sent > 0)
            {
                break Some(p);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let pair = pair.expect("no nominated candidate pair with traffic appeared");

        // The whole point of the field: before it, "selected" had to be inferred
        // from state plus priority.
        assert_eq!(
            pair.state,
            IceCandidatePairState::Succeeded,
            "the nominated pair must be a succeeded one"
        );
        assert!(pair.writable, "a nominated pair carrying bytes is writable");

        // Loopback goes host-to-host, and nothing is relayed — which is exactly
        // the answer a caller needs to be able to distinguish from a TURN path.
        assert_eq!(pair.local_candidate_type, IceCandidateType::Host);
        assert_eq!(pair.local_relay_protocol, RelayProtocol::NotRelayed);

        // Pair-level counters. Wider than the per-stream RTP ones: the data
        // channel's traffic is in here, and this connection has no media at all.
        assert!(pair.bytes_sent > 0, "nominated pair sent nothing");
        assert!(
            pair.total_round_trip_time_s >= 0.0,
            "cumulative rtt must not be negative"
        );

        println!(
            "the_nominated_pair_reports_its_candidate_type_and_counters ✅\n  \
             candidate={:?} relay={:?} bytes={}↑/{}↓ avail={:.0}↑/{:.0}↓ bps",
            pair.local_candidate_type,
            pair.local_relay_protocol,
            pair.bytes_sent,
            pair.bytes_received,
            pair.available_outgoing_bitrate_bps,
            pair.available_incoming_bitrate_bps,
        );
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
