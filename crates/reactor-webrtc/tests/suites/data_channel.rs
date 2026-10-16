#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        DataChannel, DataChannelState, IceCandidate, PeerConnection, PeerConnectionFactory,
        PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
    };

    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        data_channels: Mutex<Vec<DataChannel>>,
        // The thread on_data_channel ran on: the signaling thread.
        signaling_thread: Mutex<Option<thread::ThreadId>>,
    }

    impl Default for Peer {
        fn default() -> Self {
            Self {
                ice: Mutex::new(VecDeque::new()),
                connected: AtomicBool::new(false),
                data_channels: Mutex::new(Vec::new()),
                signaling_thread: Mutex::new(None),
            }
        }
    }

    fn factory() -> PeerConnectionFactory {
        PeerConnectionFactory::builder()
            .with_dc_callbacks_on_network_thread(super::NETWORK_THREAD_CALLBACKS)
            .build()
            .expect("factory")
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
                move |dc| {
                    *s.signaling_thread.lock().unwrap() = Some(thread::current().id());
                    s.data_channels.lock().unwrap().push(dc)
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

    // ── loopback send/receive ────────────────────────────────────────────────

    // GitHub Windows CI runners have no usable non-loopback interface for WebRTC
    // to gather host candidates on, so ICE never connects. Skip on Windows; the
    // same tests run on Linux and macOS.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn data_channel_send_receive() {
        let factory = factory();
        let cfg = RtcConfiguration::default();

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        // pc1 creates the channel before negotiation.
        let dc1 = pc1.create_data_channel("test").expect("create dc");

        let recv_count = Arc::new(AtomicU32::new(0));
        let last_msg: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        // Register dc1's on_open *before* negotiate so we don't miss the
        // transition — in a loopback test the channel can open before the
        // callback is wired if we wait until after negotiation completes.
        let dc1_open = Arc::new(AtomicBool::new(false));
        let dc1_open2 = dc1_open.clone();
        dc1.on_open(move || {
            dc1_open2.store(true, Ordering::SeqCst);
        });

        negotiate(&pc1, &pc2);

        // Wait for connection, pc2's on_data_channel, and dc1 to open.
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            s1.connected.load(Ordering::SeqCst)
                && s2.connected.load(Ordering::SeqCst)
                && !s2.data_channels.lock().unwrap().is_empty()
                && dc1_open.load(Ordering::SeqCst)
        });
        assert!(ok, "timed out waiting for connection + dc1 open");

        // Wire up the receiver on pc2's side.
        let dc2 = s2.data_channels.lock().unwrap().pop().unwrap();
        let recv_count2 = recv_count.clone();
        let last_msg2 = last_msg.clone();
        dc2.on_message(move |data, _binary| {
            *last_msg2.lock().unwrap() = data.to_vec();
            recv_count2.fetch_add(1, Ordering::SeqCst);
        });

        dc1.send(b"hello reactor", true).expect("send");
        dc1.send(b"second message", false).expect("send");

        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            recv_count.load(Ordering::SeqCst) >= 2
        });
        assert!(ok, "timed out waiting for messages");

        assert_eq!(&*last_msg.lock().unwrap(), b"second message");
        println!(
            "data_channel_send_receive ✅ — {} message(s) delivered",
            recv_count.load(Ordering::SeqCst)
        );
    }

    // ── replies from a callback ──────────────────────────────────────────────

    // A reply sent from on_message is what a command round trip does. With
    // network-thread callbacks it sends inline, on the thread that runs the
    // callback; with the default ones the callbacks share the signaling thread
    // with on_data_channel.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn replies_sent_from_on_message_arrive_and_callbacks_run_where_configured() {
        const ROUNDS: u32 = 50;
        let factory = factory();
        let cfg = RtcConfiguration::default();
        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let dc1 = pc1.create_data_channel("echo").expect("create dc");
        negotiate(&pc1, &pc2);
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            dc1.state() == DataChannelState::Open && !s2.data_channels.lock().unwrap().is_empty()
        });
        assert!(ok, "timed out waiting for the channel to open");

        let dc2 = Arc::new(s2.data_channels.lock().unwrap().pop().unwrap());
        let callback_thread = Arc::new(Mutex::new(None));
        dc2.on_message({
            let dc2 = Arc::downgrade(&dc2);
            let callback_thread = callback_thread.clone();
            move |data, binary| {
                *callback_thread.lock().unwrap() = Some(thread::current().id());
                if let Some(dc2) = dc2.upgrade() {
                    dc2.send(data, binary).expect("reply");
                }
            }
        });
        let replies = Arc::new(AtomicU32::new(0));
        dc1.on_message({
            let replies = replies.clone();
            move |data, _| {
                assert_eq!(data, b"ping");
                replies.fetch_add(1, Ordering::SeqCst);
            }
        });
        for round in 1..=ROUNDS {
            dc1.send(b"ping", true).expect("send");
            let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
                replies.load(Ordering::SeqCst) >= round
            });
            assert!(ok, "reply {round} lost");
        }

        let on_message = callback_thread.lock().unwrap().expect("on_message ran");
        let signaling = s2.signaling_thread.lock().unwrap().expect("on_data_channel ran");
        assert_eq!(
            on_message != signaling,
            super::NETWORK_THREAD_CALLBACKS,
            "on_message ran on {on_message:?}, the signaling thread is {signaling:?}"
        );
    }

    // ── state transitions ────────────────────────────────────────────────────

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn data_channel_state_transitions() {
        let factory = factory();
        let cfg = RtcConfiguration::default();

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        let dc1 = pc1.create_data_channel("state-test").expect("dc");

        let states: Arc<Mutex<Vec<DataChannelState>>> = Arc::new(Mutex::new(Vec::new()));
        let states2 = states.clone();
        dc1.on_state_change(move |s| {
            states2.lock().unwrap().push(s);
        });

        negotiate(&pc1, &pc2);

        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            states.lock().unwrap().contains(&DataChannelState::Open)
        });
        assert!(ok, "channel never reached Open");

        assert!(
            states
                .lock()
                .unwrap()
                .iter()
                .any(|s| *s == DataChannelState::Open),
            "Open state not observed"
        );
        println!(
            "data_channel_state_transitions ✅ — states: {:?}",
            states.lock().unwrap()
        );
    }

    // ── label and buffered_amount ────────────────────────────────────────────

    #[test]
    fn data_channel_label_and_buffered_amount() {
        let factory = factory();
        let cfg = RtcConfiguration::default();

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        let dc1 = pc1.create_data_channel("my-channel").expect("dc");
        assert_eq!(dc1.label(), "my-channel");

        negotiate(&pc1, &pc2);
        wait_for(&s1, &s2, &pc1, &pc2, || s1.connected.load(Ordering::SeqCst));

        // buffered_amount is 0 when nothing is queued.
        assert_eq!(dc1.buffered_amount(), 0);
        println!("data_channel_label_and_buffered_amount ✅");
    }
}
