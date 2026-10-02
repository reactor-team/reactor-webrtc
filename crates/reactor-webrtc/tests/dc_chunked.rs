//! Chunked data channels end to end: messages of any size arrive whole and
//! intact, the queue paces sends past libwebrtc's 16 MiB buffer, limits are
//! enforced, and a plain channel is untouched. Two peers on loopback. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test --test dc_chunked -- --nocapture
//! ```

#[cfg(have_libwebrtc)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        DataChannel, DataChannelState, DcChunking, DcSendError, Error, IceCandidate,
        PeerConnection, PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState,
        RtcConfiguration,
    };

    const MIB: usize = 1024 * 1024;

    fn factory(settings: Option<DcChunking>) -> PeerConnectionFactory {
        let builder = PeerConnectionFactory::builder();
        match settings {
            Some(s) => builder.with_dc_chunking(s),
            None => builder,
        }
        .build()
        .expect("factory")
    }

    fn chunking() -> PeerConnectionFactory {
        factory(Some(DcChunking::default()))
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

    /// Release the queue's lock before add_ice_candidate, which waits on the
    /// signaling thread; holding it deadlocks against the next candidate.
    fn trickle(from: &Peer, to: &PeerConnection) {
        loop {
            let next = from.ice.lock().unwrap().pop_front();
            let Some(c) = next else { break };
            let _ = to.add_ice_candidate(&c);
        }
    }

    struct Pair {
        _pc1: PeerConnection,
        _pc2: PeerConnection,
        /// The offerer's channel.
        a: DataChannel,
        /// The answerer's channel, opened by the peer.
        b: DataChannel,
    }

    fn connect(f1: &PeerConnectionFactory, f2: &PeerConnectionFactory) -> Pair {
        let (pc1, s1) = make_peer(f1);
        let (pc2, s2) = make_peer(f2);
        let a = pc1.create_data_channel("data").expect("dc");
        let offer = pc1.create_offer().expect("offer");
        pc1.set_local_description(&offer).expect("pc1 local");
        pc2.set_remote_description(&offer).expect("pc2 remote");
        let answer = pc2.create_answer().expect("answer");
        pc2.set_local_description(&answer).expect("pc2 local");
        pc1.set_remote_description(&answer).expect("pc1 remote");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let open = a.state() == DataChannelState::Open
                && s2
                    .data_channels
                    .lock()
                    .unwrap()
                    .first()
                    .is_some_and(|dc| dc.state() == DataChannelState::Open);
            if open {
                break;
            }
            assert!(Instant::now() < deadline, "peers did not connect");
            thread::sleep(Duration::from_millis(20));
        }
        let b = s2.data_channels.lock().unwrap().pop().unwrap();
        Pair {
            _pc1: pc1,
            _pc2: pc2,
            a,
            b,
        }
    }

    /// Collect every message `dc` receives.
    fn inbox(dc: &mut DataChannel) -> mpsc::Receiver<(Vec<u8>, bool)> {
        let (tx, rx) = mpsc::channel();
        dc.on_message(move |data, binary| {
            let _ = tx.send((data.to_vec(), binary));
        });
        rx
    }

    /// Bytes that differ between messages and positions, so a reordered,
    /// duplicated or lost frame shows up as a mismatch.
    fn pattern(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn messages_of_every_size_arrive_whole_both_ways() {
        let (f1, f2) = (chunking(), chunking());
        let mut p = connect(&f1, &f2);
        assert!(p.a.is_chunked() && p.b.is_chunked());
        let at_b = inbox(&mut p.b);
        let at_a = inbox(&mut p.a);
        let sizes = [
            0,
            1,
            64 * 1024 - 2,
            64 * 1024 - 1,
            64 * 1024,
            64 * 1024 + 1,
            MIB,
            20 * MIB,
            60 * MIB,
        ];
        for (i, &n) in sizes.iter().enumerate() {
            let msg = pattern(i as u64, n);
            p.a.send(&msg, true).expect("send a→b");
            let (got, binary) = at_b
                .recv_timeout(Duration::from_secs(60))
                .expect("a→b arrives");
            assert!(binary);
            assert!(got == msg, "a→b {n} bytes: content differs");

            p.b.send(&msg, true).expect("send b→a");
            let (got, _) = at_a
                .recv_timeout(Duration::from_secs(60))
                .expect("b→a arrives");
            assert!(got == msg, "b→a {n} bytes: content differs");
        }
        assert_eq!(p.a.state(), DataChannelState::Open);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn a_200_mb_message_passes_without_closing_the_channel() {
        let mut big = DcChunking::default();
        big.max_message_size = 256 * MIB as u64;
        big.send_buffer_limit = 256 * MIB as u64;
        let (f1, f2) = (factory(Some(big.clone())), factory(Some(big)));
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        let msg = pattern(200, 200 * 1000 * 1000);
        let t0 = Instant::now();
        p.a.send(&msg, true).expect("send");
        assert!(
            p.a.buffered_amount() > 16 * MIB as u64,
            "the queue holds what libwebrtc cannot"
        );
        let (got, _) = at_b
            .recv_timeout(Duration::from_secs(300))
            .expect("arrives");
        println!(
            "200 MB over loopback in {:.1} s",
            t0.elapsed().as_secs_f64()
        );
        assert!(got == msg, "content differs");
        assert_eq!(p.a.state(), DataChannelState::Open);
        assert_eq!(p.b.state(), DataChannelState::Open);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn text_split_across_frames_arrives_as_text() {
        let mut small_frames = DcChunking::default();
        small_frames.chunk_size = 1024;
        let (f1, f2) = (factory(Some(small_frames)), chunking());
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        // 3-byte characters with 1023-byte frame payloads: characters are cut.
        let text = "€uro ünïcödé ✓ ".repeat(4000);
        p.a.send(text.as_bytes(), false).expect("send");
        let (got, binary) = at_b.recv_timeout(Duration::from_secs(30)).expect("arrives");
        assert!(!binary, "a text message is delivered as text");
        assert_eq!(String::from_utf8(got).expect("valid UTF-8"), text);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn concurrent_senders_never_interleave_frames() {
        let (f1, f2) = (chunking(), chunking());
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        let a = Arc::new(p.a);
        let senders: Vec<_> = (0..4u64)
            .map(|t| {
                let a = a.clone();
                thread::spawn(move || {
                    for i in 0..25u64 {
                        let n = 1 + ((t * 7919 + i * 104_729) % (3 * MIB as u64)) as usize;
                        let mut msg = pattern(t * 1000 + i, n);
                        msg[0] = t as u8;
                        a.send(&msg, true).expect("send");
                    }
                })
            })
            .collect();
        for s in senders {
            s.join().unwrap();
        }
        let mut next = [0u64; 4];
        for _ in 0..100 {
            let (got, _) = at_b.recv_timeout(Duration::from_secs(60)).expect("arrives");
            let t = got[0] as u64;
            let i = next[t as usize];
            let n = 1 + ((t * 7919 + i * 104_729) % (3 * MIB as u64)) as usize;
            let mut want = pattern(t * 1000 + i, n);
            want[0] = t as u8;
            assert!(got == want, "sender {t} message {i}: content differs");
            next[t as usize] += 1;
        }
        assert_eq!(next, [25; 4]);
    }

    /// Small messages from many threads keep libwebrtc's buffer far below
    /// the low-water mark, so no native callback would come to move a
    /// message a lost pump request left behind: every one must arrive.
    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn small_concurrent_sends_are_never_left_queued() {
        const THREADS: u64 = 8;
        const EACH: u64 = 2_000;
        let (f1, f2) = (chunking(), chunking());
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        let a = Arc::new(p.a);
        let senders: Vec<_> = (0..THREADS)
            .map(|t| {
                let a = a.clone();
                thread::spawn(move || {
                    for i in 0..EACH {
                        a.send(&[t as u8, (i % 251) as u8], true).expect("send");
                    }
                })
            })
            .collect();
        for s in senders {
            s.join().unwrap();
        }
        for n in 0..THREADS * EACH {
            at_b.recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| {
                    panic!("message {n} never arrived; queued {}", a.buffered_amount())
                });
        }
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn buffered_amount_counts_the_queue_and_drain_waits_for_it() {
        let (f1, f2) = (chunking(), chunking());
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        p.a.send(&pattern(1, 50 * MIB), true).expect("send");
        assert!(p.a.buffered_amount() > 16 * MIB as u64);
        assert!(p.a.drain(Duration::from_secs(60)), "drained");
        assert_eq!(p.a.buffered_amount(), 0);
        at_b.recv_timeout(Duration::from_secs(60)).expect("arrives");
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn the_peers_smaller_limit_applies_and_the_channel_survives_a_refusal() {
        let mut small = DcChunking::default();
        small.max_message_size = MIB as u64;
        let (f1, f2) = (chunking(), factory(Some(small)));
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        match p.a.send(&vec![0; 2 * MIB], true) {
            Err(Error::DataChannel(DcSendError::TooLarge { size, max })) => {
                assert_eq!((size, max), (2 * MIB as u64, MIB as u64));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        p.a.send(&pattern(7, MIB), true)
            .expect("a message at the limit");
        let (got, _) = at_b.recv_timeout(Duration::from_secs(30)).expect("arrives");
        assert!(got == pattern(7, MIB));
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn a_full_queue_refuses_without_breaking_the_channel() {
        let mut tight = DcChunking::default();
        tight.max_message_size = 4 * MIB as u64;
        tight.send_buffer_limit = 4 * MIB as u64;
        let (f1, f2) = (factory(Some(tight)), chunking());
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        // Built up front: generating 3 MiB per send would give loopback time
        // to drain between sends, and the queue would never fill.
        let messages: Vec<Vec<u8>> = (0..12).map(|i| pattern(i, 3 * MIB)).collect();
        let mut accepted = 0;
        let mut refused = 0;
        for msg in &messages {
            match p.a.send(msg, true) {
                Ok(()) => accepted += 1,
                Err(Error::DataChannel(DcSendError::QueueFull { .. })) => refused += 1,
                Err(e) => panic!("unexpected error {e}"),
            }
            // The pump never lets libwebrtc's buffer pass high-water + one
            // frame; the rest is the queue, bounded by its 4 MiB limit.
            assert!(p.a.buffered_amount() <= (8 + 4) * MIB as u64 + 64 * 1024);
        }
        assert!(
            refused > 0,
            "a 4 MiB queue must refuse part of 36 MiB sent at once"
        );
        for _ in 0..accepted {
            at_b.recv_timeout(Duration::from_secs(30))
                .expect("every accepted message arrives");
        }
        assert_eq!(p.a.state(), DataChannelState::Open);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn close_sends_what_was_queued_first() {
        let (f1, f2) = (chunking(), chunking());
        let mut p = connect(&f1, &f2);
        let at_b = inbox(&mut p.b);
        let msg = pattern(9, 30 * MIB);
        p.a.send(&msg, true).expect("send");
        p.a.close(Duration::from_secs(60));
        let (got, _) = at_b
            .recv_timeout(Duration::from_secs(60))
            .expect("the queued message arrives");
        assert!(got == msg);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn messages_that_arrive_before_on_message_are_kept() {
        let (f1, f2) = (chunking(), chunking());
        let mut p = connect(&f1, &f2);
        assert!(p.b.is_chunked());
        for i in 0..3 {
            p.a.send(&pattern(i, 200 * 1024), true).expect("send");
        }
        assert!(p.a.drain(Duration::from_secs(30)));
        thread::sleep(Duration::from_millis(300));
        let at_b = inbox(&mut p.b);
        for i in 0..3 {
            let (got, _) = at_b
                .recv_timeout(Duration::from_secs(10))
                .expect("held message");
            assert!(got == pattern(i, 200 * 1024), "message {i} in order");
        }
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn a_plain_channel_behaves_as_before() {
        let (f1, f2) = (factory(None), factory(None));
        let mut p = connect(&f1, &f2);
        assert!(!p.a.is_chunked() && !p.b.is_chunked());
        let at_b = inbox(&mut p.b);
        let msg = pattern(3, 100 * 1024);
        p.a.send(&msg, true).expect("send");
        let (got, binary) = at_b.recv_timeout(Duration::from_secs(10)).expect("arrives");
        assert!(binary && got == msg);
        p.a.send(b"text", false).expect("send");
        let (got, binary) = at_b.recv_timeout(Duration::from_secs(10)).expect("arrives");
        assert!(!binary && got == b"text");
    }
}
