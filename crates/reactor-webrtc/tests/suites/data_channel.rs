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

    // ── calls back into the channel from its handlers ────────────────────────

    /// A deadlocked callback thread would hang the teardown after a failed
    /// assert, so a timeout here ends the run instead.
    fn or_exit(ok: bool, what: &str) {
        if !ok {
            eprintln!("{what}");
            std::process::exit(1);
        }
    }

    // With network-thread callbacks `close` runs inline and changes the state
    // while on_state_change(Open) is still running; the change waits for it.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn close_from_on_state_change_reports_closing_and_closed_after_open() {
        let factory = factory();
        let cfg = RtcConfiguration::default();
        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let dc1 = Arc::new(pc1.create_data_channel("close-on-open").expect("dc"));
        let states = Arc::new(Mutex::new(Vec::new()));
        dc1.on_state_change({
            let dc1 = Arc::downgrade(&dc1);
            let states = states.clone();
            move |s| {
                states.lock().unwrap().push(s);
                if s == DataChannelState::Open {
                    if let Some(dc1) = dc1.upgrade() {
                        dc1.close(Duration::ZERO);
                    }
                }
            }
        });
        negotiate(&pc1, &pc2);
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            states.lock().unwrap().contains(&DataChannelState::Closed)
        });
        or_exit(ok, "close from on_state_change(Open) never reached Closed");
        let states = states.lock().unwrap();
        assert!(
            states.ends_with(&[
                DataChannelState::Open,
                DataChannelState::Closing,
                DataChannelState::Closed
            ]),
            "states: {states:?}"
        );
    }

    // The refill pattern: with network-thread callbacks a send can report the
    // buffer draining before it returns, from inside the handler that sent.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn send_from_on_buffered_amount_low_refills_the_channel() {
        const MESSAGES: u32 = 64;
        const HIGH: u64 = 1024 * 1024;
        let payload = vec![7u8; 64 * 1024];
        fn refill(dc: &DataChannel, sent: &AtomicU32, payload: &[u8]) {
            while dc.buffered_amount() < HIGH {
                let reserved =
                    sent.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        (n < MESSAGES).then_some(n + 1)
                    });
                if reserved.is_err() {
                    return;
                }
                dc.send(payload, true).expect("send");
            }
        }

        let factory = factory();
        let cfg = RtcConfiguration::default();
        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let dc1 = Arc::new(pc1.create_data_channel("refill").expect("dc"));
        negotiate(&pc1, &pc2);
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            dc1.state() == DataChannelState::Open && !s2.data_channels.lock().unwrap().is_empty()
        });
        or_exit(ok, "timed out waiting for the channel to open");
        let dc2 = s2.data_channels.lock().unwrap().pop().unwrap();
        let received = Arc::new(AtomicU32::new(0));
        dc2.on_message({
            let received = received.clone();
            move |_, _| {
                received.fetch_add(1, Ordering::SeqCst);
            }
        });

        let sent = Arc::new(AtomicU32::new(0));
        dc1.set_buffered_amount_low_threshold(256 * 1024);
        dc1.on_buffered_amount_low({
            let dc1 = Arc::downgrade(&dc1);
            let sent = sent.clone();
            let payload = payload.clone();
            move || {
                if let Some(dc1) = dc1.upgrade() {
                    refill(&dc1, &sent, &payload);
                }
            }
        });
        refill(&dc1, &sent, &payload);
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            received.load(Ordering::SeqCst) == MESSAGES
        });
        or_exit(
            ok,
            &format!(
                "refilling from on_buffered_amount_low stalled: {} sent, {} received",
                sent.load(Ordering::SeqCst),
                received.load(Ordering::SeqCst)
            ),
        );
    }

    // A callback can end up holding the last handle, here by taking it out of
    // a holder. The release must wait for the callback to return and must not
    // unregister on the network thread.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn dropping_the_last_handle_inside_a_callback_releases_the_channel() {
        let factory = factory();
        let cfg = RtcConfiguration::default();
        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);
        let released1 = pc1.create_data_channel("released").expect("dc");
        let kept1 = pc1.create_data_channel("kept").expect("dc");
        negotiate(&pc1, &pc2);
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            released1.state() == DataChannelState::Open
                && kept1.state() == DataChannelState::Open
                && s2.data_channels.lock().unwrap().len() == 2
        });
        or_exit(ok, "timed out waiting for both channels to open");
        let mut channels = std::mem::take(&mut *s2.data_channels.lock().unwrap());
        let i = channels
            .iter()
            .position(|dc| dc.label() == "released")
            .expect("the released channel");
        let holder = Arc::new(Mutex::new(Some(channels.remove(i))));
        let kept2 = channels.pop().expect("the kept channel");

        let dropped = Arc::new(AtomicBool::new(false));
        holder.lock().unwrap().as_ref().unwrap().on_message({
            let holder = holder.clone();
            let dropped = dropped.clone();
            move |_, _| {
                let last = holder.lock().unwrap().take();
                drop(last);
                dropped.store(true, Ordering::SeqCst);
            }
        });
        let received = Arc::new(AtomicU32::new(0));
        kept2.on_message({
            let received = received.clone();
            move |_, _| {
                received.fetch_add(1, Ordering::SeqCst);
            }
        });

        released1.send(b"release", true).expect("send");
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || dropped.load(Ordering::SeqCst));
        or_exit(ok, "the callback never released the channel");
        // The callbacks' thread is still free: another channel still delivers.
        kept1.send(b"still here", true).expect("send");
        let ok = wait_for(&s1, &s2, &pc1, &pc2, || {
            received.load(Ordering::SeqCst) == 1
        });
        or_exit(ok, "no message arrived after the release");
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
