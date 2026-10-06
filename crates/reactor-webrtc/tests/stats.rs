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
        IceCandidate, IceCandidatePairState, IceCandidateType, MediaKind, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RelayProtocol,
        RtcConfiguration, StreamKind, TransceiverDirection, VideoFrame,
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
        // Pop in a block so the guard drops before add_ice_candidate, which
        // waits on the signaling thread that may be waiting for this lock.
        while let Some(c) = {
            let mut q = from.ice.lock().unwrap();
            q.pop_front()
        } {
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
        //
        // It also polls for the pair's state. libwebrtc keeps checking the
        // selected pair after nominating it, and while one of those checks is
        // out the pair reports `InProgress` — so a single snapshot can catch the
        // live pair mid-check. What has to hold is that the nominated pair
        // carrying traffic reaches `Succeeded`, which is what a reader picking
        // the live pair (nominated *and* succeeded) relies on.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_seen = None;
        let pair = loop {
            let report = pc1.get_stats().expect("get_stats");
            if let Some(p) = report
                .candidate_pairs
                .into_iter()
                .find(|p| p.nominated && p.bytes_sent > 0)
            {
                if p.state == IceCandidatePairState::Succeeded {
                    break p;
                }
                last_seen = Some(p.state);
            }
            assert!(
                Instant::now() < deadline,
                "no nominated, succeeded candidate pair with traffic appeared \
                 (last nominated pair with traffic was {last_seen:?})"
            );
            thread::sleep(Duration::from_millis(50));
        };

        assert!(pair.writable, "a nominated pair carrying bytes is writable");

        // Loopback goes host-to-host, and nothing is relayed — which is exactly
        // the answer a caller needs to be able to distinguish from a TURN path.
        assert_eq!(pair.local_candidate_type, IceCandidateType::Host);
        assert_eq!(pair.local_relay_protocol, RelayProtocol::NotRelayed);

        // Pair-level counters. Wider than the per-stream RTP ones: the data
        // channel's traffic is in here, and this connection has no media at all.
        assert!(pair.bytes_sent > 0, "nominated pair sent nothing");
        // Wired to the pair's own 64-bit counters, not to the inbound stream's
        // 32-bit `packets_received`. Reading the wrong field reports zero here,
        // because this connection carries no RTP at all.
        assert!(
            pair.packets_sent > 0,
            "nominated pair reported no packets sent"
        );
        assert!(
            pair.packets_received > 0,
            "nominated pair reported no packets received"
        );
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

    /// Sends real video between two peers and checks the per-stage latency
    /// fields: the cumulative totals on both sides, and a timing frame on the
    /// receiver whose stamps are in order within each side.
    #[test]
    fn video_stats_report_per_stage_latency() {
        const W: u32 = 320;
        const H: u32 = 240;
        let factory = PeerConnectionFactory::builder().build().expect("factory");
        let cfg = RtcConfiguration::default();

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let tx = pc1
            .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
            .expect("send transceiver");
        let video = factory
            .create_video_track("latency-video")
            .expect("video track");
        tx.set_track(&video).expect("set track");
        negotiate(&pc1, &pc2);

        let stop = AtomicBool::new(false);
        let (inbound, outbound) = thread::scope(|scope| {
            scope.spawn(|| {
                let mut seed = 0u8;
                while !stop.load(Ordering::SeqCst) {
                    // A frame that changes every time, so the encoder has real
                    // work and produces frames large enough to packetize.
                    let bgra: Vec<u8> = (0..W * H * 4)
                        .map(|i| (i as u8).wrapping_mul(seed | 1))
                        .collect();
                    let _ = video.push_frame(VideoFrame::new(&bgra, W, H));
                    seed = seed.wrapping_add(3);
                    thread::sleep(Duration::from_millis(33));
                }
            });

            // Timing frames are marked about once a second, so give the first
            // one a few seconds to arrive after the connection comes up.
            let mut found = None;
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                trickle(&s1, &pc2);
                trickle(&s2, &pc1);
                let inbound = pc2
                    .get_stats()
                    .expect("receiver stats")
                    .inbound_rtp
                    .into_iter()
                    .find(|s| s.kind == StreamKind::Video);
                let outbound = pc1
                    .get_stats()
                    .expect("sender stats")
                    .outbound_rtp
                    .into_iter()
                    .find(|s| s.kind == StreamKind::Video);
                if let (Some(i), Some(o)) = (inbound, outbound) {
                    if i.timing_frame.is_some() && i.frames_decoded >= 30 && o.frames_encoded >= 30
                    {
                        found = Some((i, o));
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
            stop.store(true, Ordering::SeqCst);
            found.expect("no timing frame and 30 decoded frames within 20 s")
        });

        assert!(
            outbound.total_encode_time_s > 0.0,
            "no encode time on the sender"
        );
        assert!(outbound.total_packet_send_delay_s >= 0.0);
        assert!(
            inbound.jitter_buffer_emitted_count > 0,
            "nothing left the jitter buffer"
        );
        assert!(
            inbound.jitter_buffer_delay_s > 0.0,
            "no jitter buffer delay"
        );
        assert!(
            inbound.total_processing_delay_s > 0.0,
            "no processing delay"
        );

        let t = inbound.timing_frame.expect("timing frame");
        let (s, r) = (t.sender, t.receiver);
        assert!(
            s.encode_finish_ms >= s.encode_start_ms,
            "encode ran backwards: {t:?}"
        );
        assert!(
            s.packetization_finish_ms >= s.encode_finish_ms,
            "packetized before encoded: {t:?}"
        );
        assert!(
            s.pacer_exit_ms >= s.packetization_finish_ms,
            "paced before packetized: {t:?}"
        );
        assert!(
            r.receive_finish_ms >= r.receive_start_ms,
            "received backwards: {t:?}"
        );
        assert!(
            r.decode_start_ms >= r.receive_finish_ms,
            "decoded before received: {t:?}"
        );
        assert!(
            r.decode_finish_ms >= r.decode_start_ms,
            "decode ran backwards: {t:?}"
        );

        println!(
            "video_stats_report_per_stage_latency ✅\n  \
             encode {:.2} ms/frame, pacer {:.3} ms/packet, jitter buffer {:.2} ms/frame, \
             decode {:.2} ms/frame\n  timing frame: {t:?}",
            outbound.total_encode_time_s / outbound.frames_encoded as f64 * 1000.0,
            outbound.total_packet_send_delay_s / outbound.packets_sent as f64 * 1000.0,
            inbound.jitter_buffer_delay_s / inbound.jitter_buffer_emitted_count as f64 * 1000.0,
            inbound.total_decode_time_s / inbound.frames_decoded as f64 * 1000.0,
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
