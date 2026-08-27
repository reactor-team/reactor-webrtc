//! Data-channel round-trip over the safe API: pc1 opens a channel, pc2 receives
//! it via `on_data_channel`, and they exchange a ping/pong.
//!
//! Gated on a native libwebrtc being linked (see build.rs).
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    DataChannel, IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RtcConfiguration,
};

#[derive(Default)]
struct Ice {
    q: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
}

fn ice_observer(ice: &Arc<Ice>) -> PeerConnectionObserver {
    PeerConnectionObserver::new()
        .on_ice_candidate({
            let i = ice.clone();
            move |c| i.q.lock().unwrap().push_back(c)
        })
        .on_connection_state_change({
            let i = ice.clone();
            move |s| {
                if s == PeerConnectionState::Connected {
                    i.connected.store(true, Ordering::SeqCst);
                }
            }
        })
}

fn forward_ice(from: &Ice, to: &PeerConnection) {
    while let Some(c) = {
        let mut q = from.q.lock().unwrap();
        q.pop_front()
    } {
        let _ = to.add_ice_candidate(&c);
    }
}

fn wire_channel(dc: &mut DataChannel, open: &Arc<AtomicBool>, inbox: &Arc<Mutex<Vec<Vec<u8>>>>) {
    dc.on_open({
        let open = open.clone();
        move || open.store(true, Ordering::SeqCst)
    });
    dc.on_message({
        let inbox = inbox.clone();
        move |data, _binary| inbox.lock().unwrap().push(data.to_vec())
    });
}

fn wait_for(predicate: impl Fn() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[test]
fn data_channel_round_trip() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    let ice1 = Arc::new(Ice::default());
    let ice2 = Arc::new(Ice::default());

    // pc2 receives the channel pc1 creates.
    let open2 = Arc::new(AtomicBool::new(false));
    let inbox2 = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let received: Arc<Mutex<Option<DataChannel>>> = Arc::new(Mutex::new(None));
    let observer2 = ice_observer(&ice2).on_data_channel({
        let open2 = open2.clone();
        let inbox2 = inbox2.clone();
        let received = received.clone();
        move |mut dc| {
            wire_channel(&mut dc, &open2, &inbox2);
            *received.lock().unwrap() = Some(dc);
        }
    });
    let pc2 = factory
        .create_peer_connection(&config, observer2)
        .expect("pc2");
    let pc1 = factory
        .create_peer_connection(&config, ice_observer(&ice1))
        .expect("pc1");

    let open1 = Arc::new(AtomicBool::new(false));
    let inbox1 = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let mut dc1 = pc1.create_data_channel("chat").expect("data channel");
    wire_channel(&mut dc1, &open1, &inbox1);

    // Negotiate.
    let offer = pc1.create_offer().expect("offer");
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");
    let answer = pc2.create_answer().expect("answer");
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                forward_ice(&ice1, &pc2);
                forward_ice(&ice2, &pc1);
                thread::sleep(Duration::from_millis(20));
            }
        });

        // Both channels open → ping → pong.
        assert!(
            wait_for(
                || open1.load(Ordering::SeqCst) && open2.load(Ordering::SeqCst),
                Duration::from_secs(20),
            ),
            "data channels did not open",
        );
        dc1.send(b"ping", false).expect("send ping");
        assert!(
            wait_for(
                || !inbox2.lock().unwrap().is_empty(),
                Duration::from_secs(5)
            ),
            "pc2 did not receive the ping",
        );
        received
            .lock()
            .unwrap()
            .as_ref()
            .expect("received channel")
            .send(b"pong", false)
            .expect("send pong");
        assert!(
            wait_for(
                || !inbox1.lock().unwrap().is_empty(),
                Duration::from_secs(5)
            ),
            "pc1 did not receive the pong",
        );
        stop.store(true, Ordering::SeqCst);
    });

    assert_eq!(inbox2.lock().unwrap()[0], b"ping");
    assert_eq!(inbox1.lock().unwrap()[0], b"pong");
    println!("data channel round-trip OK (ping → pong)");

    // Drop the received channel before its peer connection.
    received.lock().unwrap().take();
}
