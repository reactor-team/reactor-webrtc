//! Data channel message latency and throughput over loopback.
//!
//! Two peers, each with its own factory (so its own network thread, as a
//! client and a runtime in separate processes would have), exchange messages
//! over an SDP-negotiated data channel.
//!
//! - serial: send one message, wait until the peer's on_message has it whole,
//!   repeat. Reports the one-way latency distribution.
//! - stream: send every message back to back. Reports throughput.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo run --release --example dc_bench -- \
//!     [messages] [message_bytes]
//! ```
//!
//! BENCH_CHUNKED=1 chunks the channel (DcChunking defaults); BENCH_WINDOW sets
//! the messages in flight while streaming (default 16); BENCH_SERIAL_ONLY=1
//! skips the stream.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    DataChannel, DataChannelState, IceCandidate, PeerConnection, PeerConnectionFactory,
    PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
};

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
    while let Some(c) = {
        let mut q = from.ice.lock().unwrap();
        q.pop_front()
    } {
        let _ = to.add_ice_candidate(&c);
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let messages: usize = args.get(1).map_or(2000, |s| s.parse().unwrap());
    let size: usize = args.get(2).map_or(100 * 1024, |s| s.parse().unwrap());

    // BENCH_CHUNKED=1: both factories chunk with the defaults (max_burst 256).
    let factory = || {
        let b = PeerConnectionFactory::builder();
        if std::env::var_os("BENCH_CHUNKED").is_some() {
            b.with_dc_chunking(reactor_webrtc::DcChunking::default())
        } else {
            b
        }
        .build()
        .expect("factory")
    };
    let (f1, f2) = (factory(), factory());
    let (pc1, s1) = make_peer(&f1);
    let (pc2, s2) = make_peer(&f2);
    let dc1 = pc1.create_data_channel("bench").expect("create dc");

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
            && dc1.state() == DataChannelState::Open
            && !s2.data_channels.lock().unwrap().is_empty()
        {
            break;
        }
        assert!(Instant::now() < deadline, "did not connect");
        thread::sleep(Duration::from_millis(10));
    }
    let dc2 = s2.data_channels.lock().unwrap().remove(0);

    let received = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();
    let last_us = Arc::new(AtomicU64::new(0));
    dc2.on_message({
        let received = received.clone();
        let last_us = last_us.clone();
        move |data, _| {
            assert_eq!(data.len(), size);
            last_us.store(epoch.elapsed().as_micros() as u64, Ordering::SeqCst);
            received.fetch_add(1, Ordering::SeqCst);
        }
    });

    let payload = vec![0x5au8; size];
    println!(
        "{messages} messages of {size} B, chunked={}",
        dc1.is_chunked()
    );

    // Warm up: let congestion control open the window.
    for _ in 0..200 {
        let before = received.load(Ordering::SeqCst);
        dc1.send(&payload, true).unwrap();
        while received.load(Ordering::SeqCst) == before {
            std::hint::spin_loop();
        }
    }

    // serial
    let mut lat = Vec::with_capacity(messages);
    let mut send_call = Vec::with_capacity(messages);
    for _ in 0..messages {
        let before = received.load(Ordering::SeqCst);
        let t0 = Instant::now();
        dc1.send(&payload, true).unwrap();
        send_call.push(t0.elapsed().as_secs_f64() * 1e6);
        let wait = Instant::now() + Duration::from_secs(5);
        while received.load(Ordering::SeqCst) == before {
            assert!(Instant::now() < wait, "message lost");
            std::hint::spin_loop();
        }
        lat.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    send_call.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = lat.iter().sum::<f64>() / lat.len() as f64;
    println!(
        "serial  one-way latency us: mean {:6.0}  p50 {:6.0}  p90 {:6.0}  p99 {:6.0} | send() call us: p50 {:5.0}  p99 {:5.0}",
        mean,
        percentile(&lat, 0.5),
        percentile(&lat, 0.9),
        percentile(&lat, 0.99),
        percentile(&send_call, 0.5),
        percentile(&send_call, 0.99),
    );

    if std::env::var_os("BENCH_SERIAL_ONLY").is_some() {
        return;
    }

    // stream: keep at most `window` messages in flight so buffered_amount
    // stays bounded.
    let window: u64 = std::env::var("BENCH_WINDOW").map_or(16, |s| s.parse().unwrap());
    let start = received.load(Ordering::SeqCst);
    let t0_us = epoch.elapsed().as_micros() as u64;
    for i in 0..messages as u64 {
        while i - (received.load(Ordering::SeqCst) - start) >= window {
            std::hint::spin_loop();
        }
        dc1.send(&payload, true).unwrap();
    }
    let wait = Instant::now() + Duration::from_secs(30);
    while received.load(Ordering::SeqCst) - start < messages as u64 {
        assert!(Instant::now() < wait, "stream did not finish");
        std::hint::spin_loop();
    }
    let secs = (last_us.load(Ordering::SeqCst) - t0_us) as f64 / 1e6;
    println!(
        "stream  {:6.1} MB/s, {:5.0} us/message",
        (messages * size) as f64 / secs / 1e6,
        secs * 1e6 / messages as f64
    );
}
