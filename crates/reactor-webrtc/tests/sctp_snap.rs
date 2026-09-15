//! Integration tests for SNAP — the SCTP half of WARP
//! (`draft-hancke-tsvwg-snap`), exposed as `RtcConfiguration::sctp_snap`.
//!
//! The SDP assertions are what prove the flag reached libwebrtc: with SNAP on,
//! the data m-section carries this side's SCTP INIT parameters as
//! `a=sctp-init:`, and the peer's data channel skips the cookie exchange. Run
//! with:
//!
//! ```sh
//! REACTOR_WEBRTC_PREBUILT_URL=... cargo test --test sctp_snap -- --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        DataChannel, IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
        PeerConnectionState, RtcConfiguration,
    };

    /// The attribute SNAP defines for the SCTP INIT parameters.
    const SCTP_INIT: &str = "a=sctp-init:";

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        data_channels: Mutex<Vec<DataChannel>>,
    }

    fn snap(enabled: bool) -> RtcConfiguration {
        RtcConfiguration {
            sctp_snap: enabled,
            ..Default::default()
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

    fn trickle(from: &Peer, to: &PeerConnection) {
        while let Some(c) = from.ice.lock().unwrap().pop_front() {
            let _ = to.add_ice_candidate(&c);
        }
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

    /// SDP-only: what the flag puts on the wire, and that it stays a
    /// both-ends agreement. No ICE, so this runs everywhere.
    #[test]
    fn snap_puts_the_init_params_in_the_sdp() {
        let factory = PeerConnectionFactory::builder().build().expect("factory");

        let (off, _) = make_peer(&factory, &snap(false));
        off.create_data_channel("plain").expect("dc");
        let plain_offer = off.create_offer().expect("offer");
        assert!(
            !plain_offer.sdp.contains(SCTP_INIT),
            "SNAP is off by default, so no {SCTP_INIT} belongs in the offer:\n{}",
            plain_offer.sdp
        );

        let (on, _) = make_peer(&factory, &snap(true));
        on.create_data_channel("accelerated").expect("dc");
        let snap_offer = on.create_offer().expect("offer");
        assert!(
            snap_offer.sdp.contains(SCTP_INIT),
            "sctp_snap = true must put {SCTP_INIT} in the data m-section:\n{}",
            snap_offer.sdp
        );

        println!("snap_puts_the_init_params_in_the_sdp ✅");
    }

    /// An answerer that did not opt in must not mirror the attribute — SNAP is
    /// a both-ends agreement, and this is what keeps a mixed deployment sane.
    #[test]
    fn answerer_without_snap_does_not_mirror_it() {
        let factory = PeerConnectionFactory::builder().build().expect("factory");

        let (pc1, _) = make_peer(&factory, &snap(true));
        let (pc2, _) = make_peer(&factory, &snap(false));
        pc1.create_data_channel("warp").expect("dc");

        let offer = pc1.create_offer().expect("offer");
        assert!(offer.sdp.contains(SCTP_INIT));
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");

        let answer = pc2.create_answer().expect("answer");
        assert!(
            !answer.sdp.contains(SCTP_INIT),
            "a peer with SNAP off must answer without the INIT params:\n{}",
            answer.sdp
        );

        println!("answerer_without_snap_does_not_mirror_it ✅");
    }

    /// End to end: a channel negotiated with SNAP on both ends opens and
    /// carries a message. What SNAP changes is how the SCTP association is set
    /// up, so the only way to see it intact is to use it.
    ///
    /// GitHub Windows CI runners have no usable non-loopback interface for
    /// WebRTC to gather host candidates on, so ICE never connects there.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn snap_channel_opens_and_carries_data() {
        let factory = PeerConnectionFactory::builder().build().expect("factory");

        let (pc1, s1) = make_peer(&factory, &snap(true));
        let (pc2, s2) = make_peer(&factory, &snap(true));
        let dc1 = pc1.create_data_channel("warp").expect("create dc");

        let offer = pc1.create_offer().expect("offer");
        assert!(offer.sdp.contains(SCTP_INIT), "offer lost its INIT params");
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");
        let answer = pc2.create_answer().expect("answer");
        assert!(
            answer.sdp.contains(SCTP_INIT),
            "an answerer with SNAP on must mirror the INIT params:\n{}",
            answer.sdp
        );
        pc2.set_local_description(&answer).expect("pc2 local");
        pc1.set_remote_description(&answer).expect("pc1 remote");

        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            s1.connected.load(Ordering::SeqCst)
                && s2.connected.load(Ordering::SeqCst)
                && !s2.data_channels.lock().unwrap().is_empty()
        });
        assert!(
            ok,
            "timed out waiting for the connection + pc2's channel \
             (pc1 connected: {}, pc2 connected: {}, pc2 channels: {})",
            s1.connected.load(Ordering::SeqCst),
            s2.connected.load(Ordering::SeqCst),
            s2.data_channels.lock().unwrap().len()
        );

        let received = Arc::new(AtomicU32::new(0));
        let mut dc2 = s2.data_channels.lock().unwrap().pop().unwrap();
        let received2 = received.clone();
        dc2.on_message(move |_data, _binary| {
            received2.fetch_add(1, Ordering::SeqCst);
        });

        // pc2's end is open; dc1 may still be opening, so keep sending until
        // one lands.
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            let _ = dc1.send(b"warp", true);
            received.load(Ordering::SeqCst) > 0
        });
        assert!(ok, "no message arrived over the SNAP-negotiated channel");

        println!("snap_channel_opens_and_carries_data ✅");
    }
}
