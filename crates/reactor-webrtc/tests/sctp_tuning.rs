//! `PeerConnectionFactoryBuilder::with_sctp_tuning`: the factory takes the
//! settings, rejects values out of range, and two tuned peers still exchange
//! messages of every size, plain and chunked. The latency effect needs a path
//! with real delay and loss (see `webrtc-build/patches/README.md`, 0011), so
//! it is not measured here. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test --test sctp_tuning -- --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        DataChannel, DataChannelState, DcChunking, IceCandidate, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
        SctpTuning,
    };

    #[test]
    fn factory_builds_with_the_low_latency_and_the_default_tuning() {
        for tuning in [SctpTuning::low_latency(), SctpTuning::default()] {
            let factory = PeerConnectionFactory::builder()
                .with_sctp_tuning(tuning)
                .build()
                .expect("factory with sctp tuning");
            factory
                .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
                .expect("peer connection");
        }
    }

    #[test]
    fn factory_rejects_values_out_of_range() {
        let build = |f: fn(&mut SctpTuning)| {
            let mut tuning = SctpTuning::default();
            f(&mut tuning);
            PeerConnectionFactory::builder()
                .with_sctp_tuning(tuning)
                .build()
                .err()
                .map(|e| e.to_string())
        };
        assert!(build(|t| t.min_cwnd_mtus = Some(0))
            .unwrap()
            .contains("min_cwnd_mtus"));
        assert!(build(|t| t.rto_min_ms = Some(1_000_000))
            .unwrap()
            .contains("rto_min_ms"));
        assert!(build(|t| t.initial_cwnd_mtus = Some(300)).is_none());
    }

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        data_channels: Mutex<Vec<DataChannel>>,
    }

    fn make_peer(factory: &PeerConnectionFactory) -> (PeerConnection, Arc<Peer>) {
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
            .create_peer_connection(&RtcConfiguration::default(), obs)
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

    fn tuned(chunking: bool) -> PeerConnectionFactory {
        let builder = PeerConnectionFactory::builder().with_sctp_tuning(SctpTuning::low_latency());
        if chunking {
            builder.with_dc_chunking(DcChunking::default())
        } else {
            builder
        }
        .build()
        .expect("factory")
    }

    /// Two tuned peers on one channel, each message echoed back by B.
    fn round_trips(chunking: bool, sizes: &[usize]) {
        let (f1, f2) = (tuned(chunking), tuned(chunking));
        let (pc1, s1) = make_peer(&f1);
        let (pc2, s2) = make_peer(&f2);
        let a = pc1.create_data_channel("tuned").expect("create dc");
        let offer = pc1.create_offer().unwrap();
        pc1.set_local_description(&offer).unwrap();
        pc2.set_remote_description(&offer).unwrap();
        let answer = pc2.create_answer().unwrap();
        pc2.set_local_description(&answer).unwrap();
        pc1.set_remote_description(&answer).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            if s1.connected.load(Ordering::SeqCst)
                && a.state() == DataChannelState::Open
                && !s2.data_channels.lock().unwrap().is_empty()
            {
                break;
            }
            assert!(Instant::now() < deadline, "did not connect");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(a.is_chunked(), chunking);
        let b = Arc::new(s2.data_channels.lock().unwrap().remove(0));
        b.on_message({
            let b = Arc::downgrade(&b);
            move |data, binary| {
                if let Some(b) = b.upgrade() {
                    b.send(data, binary).expect("echo");
                }
            }
        });
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let tx = Mutex::new(tx);
        a.on_message(move |data, _| {
            let _ = tx.lock().unwrap().send(data.to_vec());
        });
        for &size in sizes {
            let payload: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
            a.send(&payload, true).expect("send");
            let back = rx.recv_timeout(Duration::from_secs(30)).expect("echo lost");
            assert_eq!(back, payload, "{size} B echo differs");
        }
    }

    // GitHub Windows CI runners have no usable non-loopback interface for WebRTC
    // to gather host candidates on, so ICE never connects. Skip on Windows.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn tuned_peers_exchange_messages_of_every_size_on_a_plain_channel() {
        round_trips(false, &[1, 1200, 64 * 1024, 200 * 1024]);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn tuned_peers_exchange_messages_of_every_size_on_a_chunked_channel() {
        round_trips(true, &[1, 100 * 1024, 1024 * 1024, 8 * 1024 * 1024]);
    }
}
