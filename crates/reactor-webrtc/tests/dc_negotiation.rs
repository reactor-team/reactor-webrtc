//! Data-channel chunking negotiation: the `a=x-reactor-dc-chunking` attribute
//! in offers and answers, and each channel's decision to be chunked.
//!
//! The SDP cases need no connection. The channel cases connect two peers on
//! loopback and check both ends agree; a channel decides once it is open, from
//! the negotiated SDP and its own ordered/reliable parameters. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test --test dc_negotiation -- --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        DataChannel, DataChannelState, DcChunking, IceCandidate, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
        SessionDescription,
    };

    fn chunking_factory() -> PeerConnectionFactory {
        PeerConnectionFactory::builder()
            .with_dc_chunking(DcChunking::default())
            .build()
            .expect("factory")
    }

    fn plain_factory() -> PeerConnectionFactory {
        PeerConnectionFactory::builder().build().expect("factory")
    }

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        data_channels: Mutex<Vec<DataChannel>>,
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
            })
            .on_data_channel({
                let s = shared.clone();
                move |dc| s.data_channels.lock().unwrap().push(dc)
            });
        let pc = factory
            .create_peer_connection(cfg, obs)
            .expect("peer connection");
        (pc, shared)
    }

    /// Forward queued candidates. The queue's lock is released before
    /// `add_ice_candidate`, which waits on the signaling thread: holding it
    /// would deadlock against that thread delivering the next candidate.
    fn trickle(from: &Peer, to: &PeerConnection) {
        loop {
            let next = from.ice.lock().unwrap().pop_front();
            let Some(c) = next else { break };
            let _ = to.add_ice_candidate(&c);
        }
    }

    fn offer_answer(
        pc1: &PeerConnection,
        pc2: &PeerConnection,
    ) -> (SessionDescription, SessionDescription) {
        let offer = pc1.create_offer().expect("offer");
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");
        let answer = pc2.create_answer().expect("answer");
        pc2.set_local_description(&answer).expect("pc2 local");
        pc1.set_remote_description(&answer).expect("pc1 remote");
        (offer, answer)
    }

    /// Negotiate, connect and return the offerer's channel and the answerer's.
    fn connect(
        f1: &PeerConnectionFactory,
        f2: &PeerConnectionFactory,
    ) -> (PeerConnection, PeerConnection, DataChannel, DataChannel) {
        let cfg = RtcConfiguration::default();
        let (pc1, s1) = make_peer(f1, &cfg);
        let (pc2, s2) = make_peer(f2, &cfg);
        let dc1 = pc1.create_data_channel("data").expect("dc");
        offer_answer(&pc1, &pc2);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let ready = s1.connected.load(Ordering::SeqCst)
                && s2.connected.load(Ordering::SeqCst)
                && dc1.state() == DataChannelState::Open
                && s2
                    .data_channels
                    .lock()
                    .unwrap()
                    .first()
                    .is_some_and(|dc| dc.state() == DataChannelState::Open);
            if ready {
                break;
            }
            assert!(Instant::now() < deadline, "peers did not connect");
            thread::sleep(Duration::from_millis(20));
        }
        let dc2 = s2.data_channels.lock().unwrap().pop().unwrap();
        (pc1, pc2, dc1, dc2)
    }

    // ── SDP ─────────────────────────────────────────────────────────────────

    #[test]
    fn a_chunking_factory_declares_it_in_the_offer_with_its_limit() {
        let factory = chunking_factory();
        let (pc, _) = make_peer(&factory, &RtcConfiguration::default());
        let _dc = pc.create_data_channel("probe").expect("dc");
        let offer = pc.create_offer().expect("offer");
        assert!(offer.declares_dc_chunking());
        assert!(offer.sdp.contains(&format!(
            "a=x-reactor-dc-chunking:1 max-message-size={}",
            DcChunking::default().max_message_size
        )));
    }

    #[test]
    fn a_plain_factory_or_an_opted_out_connection_declares_nothing() {
        let plain = plain_factory();
        let (pc, _) = make_peer(&plain, &RtcConfiguration::default());
        let _dc = pc.create_data_channel("probe").expect("dc");
        assert!(!pc.create_offer().expect("offer").declares_dc_chunking());

        let chunking = chunking_factory();
        let opted_out = RtcConfiguration {
            dc_chunking: false,
            ..Default::default()
        };
        let (pc, _) = make_peer(&chunking, &opted_out);
        let _dc = pc.create_data_channel("probe").expect("dc");
        assert!(!pc.create_offer().expect("offer").declares_dc_chunking());
    }

    #[test]
    fn the_answer_mirrors_only_an_offer_that_declared_it() {
        let cases = [
            (chunking_factory(), chunking_factory(), true),
            (plain_factory(), chunking_factory(), false),
            (chunking_factory(), plain_factory(), false),
            (plain_factory(), plain_factory(), false),
        ];
        for (i, (f1, f2, expected)) in cases.iter().enumerate() {
            let cfg = RtcConfiguration::default();
            let (pc1, _) = make_peer(f1, &cfg);
            let (pc2, _) = make_peer(f2, &cfg);
            let _dc = pc1.create_data_channel("probe").expect("dc");
            let (_, answer) = offer_answer(&pc1, &pc2);
            assert_eq!(answer.declares_dc_chunking(), *expected, "case {i}: answer");
            assert_eq!(pc1.dc_chunking_negotiated(), *expected, "case {i}: offerer");
            assert_eq!(
                pc2.dc_chunking_negotiated(),
                *expected,
                "case {i}: answerer"
            );
        }
    }

    // ── channels ────────────────────────────────────────────────────────────

    // GitHub Windows CI runners have no usable non-loopback interface for
    // WebRTC to gather host candidates on, so ICE never connects there.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn both_ends_agree_a_reliable_channel_is_chunked() {
        let (f1, f2) = (chunking_factory(), chunking_factory());
        let (_pc1, _pc2, dc1, dc2) = connect(&f1, &f2);
        assert!(dc1.ordered() && dc1.reliable());
        assert!(dc1.is_chunked(), "offerer's channel");
        assert!(dc2.is_chunked(), "answerer's channel (opened by the peer)");
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn a_peer_without_chunking_keeps_both_ends_plain() {
        for (f1, f2) in [
            (chunking_factory(), plain_factory()),
            (plain_factory(), chunking_factory()),
        ] {
            let (_pc1, _pc2, dc1, dc2) = connect(&f1, &f2);
            assert!(!dc1.is_chunked());
            assert!(!dc2.is_chunked());
        }
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn a_renegotiation_that_drops_the_attribute_leaves_open_channels_chunked() {
        let (f1, f2) = (chunking_factory(), chunking_factory());
        let (pc1, pc2, dc1, dc2) = connect(&f1, &f2);
        assert!(dc1.is_chunked() && dc2.is_chunked());

        // A second offer with the attribute stripped.
        let strip = |d: &SessionDescription| SessionDescription {
            kind: d.kind,
            sdp: d
                .sdp
                .lines()
                .filter(|l| !l.starts_with("a=x-reactor-dc-chunking:"))
                .map(|l| format!("{l}\r\n"))
                .collect(),
        };
        let offer = strip(&pc1.create_offer().expect("offer"));
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");
        // The answer to an offer without the attribute leaves it out too, even
        // though this connection already negotiated chunking.
        let answer = pc2.create_answer().expect("answer");
        assert!(!answer.declares_dc_chunking());
        pc2.set_local_description(&answer).expect("pc2 local");
        pc1.set_remote_description(&answer).expect("pc1 remote");

        assert!(dc1.is_chunked() && dc2.is_chunked());
        assert!(pc1.dc_chunking_negotiated() && pc2.dc_chunking_negotiated());
    }
}
