//! `PeerConnectionFactoryBuilder::with_dc_chunking`: the factory takes the
//! settings, rejects ones that cannot work, and sets dcsctp's `max_burst`.
//!
//! The `max_burst` effect is only visible over a path with real latency: with
//! upstream's 4, a single 100 KB message leaves in bursts of 4, 8, 16 …
//! packets, one per round trip; with 256 it leaves in one flight. The timed
//! test routes both peers through a UDP relay that delays every packet by
//! [`ONE_WAY`], and compares a plain factory with a chunking one. It needs a
//! libwebrtc with patch 0005 and depends on timing, so it is ignored by
//! default. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test --test dc_max_burst -- --include-ignored --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        DataChannel, DataChannelState, DcChunking, IceCandidate, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
    };

    const ONE_WAY: Duration = Duration::from_millis(20);

    #[test]
    fn factory_builds_with_default_settings() {
        let factory = PeerConnectionFactory::builder()
            .with_dc_chunking(DcChunking::default())
            .build()
            .expect("factory with dc chunking");
        let pc = factory
            .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
            .expect("peer connection");
        assert_eq!(pc.dc_chunking(), Some(&DcChunking::default()));
    }

    #[test]
    fn a_connection_can_opt_out_and_a_plain_factory_offers_nothing() {
        let chunking = PeerConnectionFactory::builder()
            .with_dc_chunking(DcChunking::default())
            .build()
            .expect("factory");
        let opted_out = RtcConfiguration {
            dc_chunking: false,
            ..Default::default()
        };
        let pc = chunking
            .create_peer_connection(&opted_out, PeerConnectionObserver::new())
            .expect("peer connection");
        assert_eq!(pc.dc_chunking(), None);

        let plain = PeerConnectionFactory::builder().build().expect("factory");
        let pc = plain
            .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
            .expect("peer connection");
        assert_eq!(pc.dc_chunking(), None);
    }

    #[test]
    fn factory_rejects_settings_that_cannot_work() {
        let build = |f: fn(&mut DcChunking)| {
            let mut settings = DcChunking::default();
            f(&mut settings);
            PeerConnectionFactory::builder()
                .with_dc_chunking(settings)
                .build()
                .err()
                .map(|e| e.to_string())
        };
        assert!(build(|s| s.max_burst = 0).unwrap().contains("max_burst"));
        assert!(build(|s| s.chunk_size = 1).unwrap().contains("chunk_size"));
        assert!(build(|s| s.send_buffer_limit = 1)
            .unwrap()
            .contains("queue_limit"));
    }

    // ── the timed max_burst test ────────────────────────────────────────────

    /// Forwards UDP between two peers with a fixed delay each way. Peer A is
    /// told B lives at `x`, B is told A lives at `y`; packets in on `x` leave
    /// from `y` and the other way round, so each peer sees the other at the
    /// address it was given.
    struct Relay {
        x: SocketAddr,
        y: SocketAddr,
    }

    impl Relay {
        fn start(ip: &str, a: SocketAddr, b: SocketAddr) -> Self {
            let x = UdpSocket::bind((ip, 0)).unwrap();
            let y = UdpSocket::bind((ip, 0)).unwrap();
            let relay = Self {
                x: x.local_addr().unwrap(),
                y: y.local_addr().unwrap(),
            };
            Self::pump(x.try_clone().unwrap(), y.try_clone().unwrap(), b);
            Self::pump(y, x, a);
            relay
        }

        fn pump(from: UdpSocket, out: UdpSocket, to: SocketAddr) {
            let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
            thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = from.recv(&mut buf) {
                    if tx
                        .send((Instant::now() + ONE_WAY, buf[..n].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
            });
            thread::spawn(move || {
                // A constant delay keeps arrival order, so FIFO is enough.
                for (due, data) in rx {
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    let _ = out.send_to(&data, to);
                }
            });
        }
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

    /// The first IPv4 UDP host candidate: `candidate:<f> <c> udp <prio> <ip> <port> typ host`.
    fn host_udp_v4(peer: &Peer) -> Option<IceCandidate> {
        peer.ice.lock().unwrap().iter().find_map(|c| {
            let f: Vec<&str> = c.candidate.split_whitespace().collect();
            (f.len() > 7
                && f[2].eq_ignore_ascii_case("udp")
                && f[7] == "host"
                && f[4].contains('.'))
            .then(|| c.clone())
        })
    }

    fn rewrite(c: &IceCandidate, addr: SocketAddr) -> IceCandidate {
        let mut f: Vec<String> = c.candidate.split_whitespace().map(str::to_owned).collect();
        f[4] = addr.ip().to_string();
        f[5] = addr.port().to_string();
        IceCandidate {
            candidate: f.join(" "),
            ..c.clone()
        }
    }

    fn addr_of(c: &IceCandidate) -> SocketAddr {
        let f: Vec<&str> = c.candidate.split_whitespace().collect();
        format!("{}:{}", f[4], f[5]).parse().unwrap()
    }

    fn wait(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Median round trip, in RTTs, of one `size`-byte message answered with
    /// one byte, through the relay.
    fn single_message_rtts(factory: &PeerConnectionFactory, size: usize) -> f64 {
        let (pc1, s1) = make_peer(factory);
        let (pc2, s2) = make_peer(factory);
        let mut dc1 = pc1.create_data_channel("data").expect("dc");
        let offer = pc1.create_offer().expect("offer");
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");
        let answer = pc2.create_answer().expect("answer");
        pc2.set_local_description(&answer).expect("pc2 local");
        pc1.set_remote_description(&answer).expect("pc1 remote");

        // Only one candidate per side is shared, rewritten to the relay.
        wait("host candidates", || {
            host_udp_v4(&s1).is_some() && host_udp_v4(&s2).is_some()
        });
        let (c1, c2) = (host_udp_v4(&s1).unwrap(), host_udp_v4(&s2).unwrap());
        let ip = addr_of(&c1).ip().to_string();
        let relay = Relay::start(&ip, addr_of(&c1), addr_of(&c2));
        pc2.add_ice_candidate(&rewrite(&c1, relay.y))
            .expect("candidate");
        pc1.add_ice_candidate(&rewrite(&c2, relay.x))
            .expect("candidate");
        wait("connected", || {
            s1.connected.load(Ordering::SeqCst)
                && s2.connected.load(Ordering::SeqCst)
                && !s2.data_channels.lock().unwrap().is_empty()
        });

        let mut dc2 = s2.data_channels.lock().unwrap().pop().unwrap();
        let (got_tx, got_rx) = mpsc::channel::<()>();
        dc2.on_message(move |_, _| {
            let _ = got_tx.send(());
        });
        let (ack_tx, ack_rx) = mpsc::channel::<Instant>();
        dc1.on_message(move |_, _| {
            let _ = ack_tx.send(Instant::now());
        });
        wait("channel open", || dc1.state() == DataChannelState::Open);

        // Large enough for the warm-up rounds as well as the measured ones.
        let payload = vec![0x5au8; size.max(255 * 1024)];
        let round = |n: usize| -> f64 {
            let t0 = Instant::now();
            dc1.send(&payload[..n], true).expect("send");
            got_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("message arrives");
            dc2.send(b"k", true).expect("ack");
            let t1 = ack_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("ack arrives");
            (t1 - t0).as_secs_f64() / (2.0 * ONE_WAY.as_secs_f64())
        };
        // Warm the congestion window first, as a long-lived connection would.
        for _ in 0..10 {
            round(255 * 1024);
        }
        let mut rtts: Vec<f64> = (0..9).map(|_| round(size)).collect();
        rtts.sort_by(f64::total_cmp);
        rtts[rtts.len() / 2]
    }

    #[test]
    #[ignore = "timed, and needs a libwebrtc with patch 0005 (prebuilt p10 or a local build)"]
    fn max_burst_lets_a_100kb_message_leave_in_one_flight() {
        let plain = PeerConnectionFactory::builder().build().expect("factory");
        let chunking = PeerConnectionFactory::builder()
            .with_dc_chunking(DcChunking::default())
            .build()
            .expect("factory");

        let before = single_message_rtts(&plain, 100 * 1024);
        let after = single_message_rtts(&chunking, 100 * 1024);
        println!(
            "100 KB single message: max_burst 4 = {before:.2} RTT, max_burst 256 = {after:.2} RTT"
        );
        assert!(
            before > 4.0,
            "upstream max_burst should take ~5 RTT, took {before:.2}"
        );
        assert!(
            after < 1.6,
            "max_burst 256 should take ~1 RTT, took {after:.2}"
        );
    }
}
