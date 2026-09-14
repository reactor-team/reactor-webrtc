//! Integration test for SPED — the DTLS half of WARP
//! (`draft-hancke-webrtc-sped`), exposed as
//! `PeerConnectionFactoryBuilder::with_dtls_in_stun`.
//!
//! Two things are worth asserting from outside libwebrtc. First, that the
//! factory builds at all: the glue turns the knob into libwebrtc's
//! `WebRTC-IceHandshakeDtls` field trial and fails the create when libwebrtc
//! rejects the string, so a successful build is proof the trial parsed and is
//! live in the factory's environment. Second, that a connection whose DTLS
//! handshake rides inside the ICE binding requests still reaches Connected and
//! carries data — whether the handshake actually took the short path is
//! libwebrtc-internal and not visible through our API. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_PREBUILT_URL=... cargo test --test dtls_in_stun -- --nocapture
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

    // GitHub Windows CI runners have no usable non-loopback interface for
    // WebRTC to gather host candidates on, so ICE never connects there.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn dtls_in_stun_factory_connects_and_carries_data() {
        let factory = PeerConnectionFactory::builder()
            .with_dtls_in_stun(true)
            .build()
            .expect("factory with the DTLS-in-STUN field trial");

        let cfg = RtcConfiguration::default();
        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let dc1 = pc1.create_data_channel("sped").expect("create dc");

        let offer = pc1.create_offer().expect("offer");
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");
        let answer = pc2.create_answer().expect("answer");
        pc2.set_local_description(&answer).expect("pc2 local");
        pc1.set_remote_description(&answer).expect("pc1 remote");

        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            s1.connected.load(Ordering::SeqCst)
                && s2.connected.load(Ordering::SeqCst)
                && !s2.data_channels.lock().unwrap().is_empty()
        });
        assert!(
            ok,
            "a factory with DTLS-in-STUN never reached Connected — the \
             piggybacked handshake must fall back to the normal one, never stall"
        );

        let received = Arc::new(AtomicU32::new(0));
        let mut dc2 = s2.data_channels.lock().unwrap().pop().unwrap();
        let received2 = received.clone();
        dc2.on_message(move |_data, _binary| {
            received2.fetch_add(1, Ordering::SeqCst);
        });

        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            let _ = dc1.send(b"sped", true);
            received.load(Ordering::SeqCst) > 0
        });
        assert!(ok, "no message arrived over the DTLS-in-STUN connection");

        println!("dtls_in_stun_factory_connects_and_carries_data ✅");
    }
}
