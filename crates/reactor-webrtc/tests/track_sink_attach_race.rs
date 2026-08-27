//! `Track::on_video_frame` moved from `&mut self` to `&self` so a shared
//! `Arc<Track>` (what the Python binding needs for `Transceiver.set_track` to
//! be awaitable) can still (re)attach a sink. That makes concurrent callers
//! possible for the first time, and the native FFI call plus the Rust-side
//! store have to be atomic *together*: if one caller's FFI registration lands
//! between another caller's FFI call and its store, the native side can end up
//! pointing at whatever `Box` the other caller's store just dropped — freed
//! memory the callback thread then reads from `video_sink_tramp`.
//!
//! Reproduces the race with real media flowing (so the native callback is
//! actually firing concurrently with reattachment, not just idle) and many
//! threads hammering `on_video_frame`. Without the fix (the lock taken only
//! around the store, not the FFI call too) this crashes non-deterministically
//! under `cargo test --release` within a handful of runs; with the fix it is
//! stable. Verified by temporarily reverting the fix and observing the crash.
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RemoteTrack, RtcConfiguration, VideoTrack,
};

#[derive(Default)]
struct Shared {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    remote_video: Mutex<Option<Arc<VideoTrack>>>,
}

fn make_peer(
    factory: &PeerConnectionFactory,
    config: &RtcConfiguration,
) -> (PeerConnection, Arc<Shared>) {
    let shared = Arc::new(Shared::default());
    let observer = PeerConnectionObserver::new()
        .on_ice_candidate({
            let s = shared.clone();
            move |c| s.ice.lock().unwrap().push_back(c)
        })
        .on_connection_state_change({
            let s = shared.clone();
            move |state| {
                if state == PeerConnectionState::Connected {
                    s.connected.store(true, Ordering::SeqCst);
                }
            }
        })
        .on_track({
            let s = shared.clone();
            move |track| {
                if let RemoteTrack::Video(v) = track {
                    *s.remote_video.lock().unwrap() = Some(Arc::new(v));
                }
            }
        });
    let pc = factory
        .create_peer_connection(config, observer)
        .expect("create peer connection");
    (pc, shared)
}

fn forward_ice(from: &Shared, to: &PeerConnection) {
    while let Some(c) = {
        let mut q = from.ice.lock().unwrap();
        q.pop_front()
    } {
        let _ = to.add_ice_candidate(&c);
    }
}

#[test]
fn concurrent_on_video_frame_attach_survives_live_frame_delivery() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let video = factory
        .create_video_track("race-video")
        .expect("video track");
    pc1.add_track(&video).expect("add video");

    let offer = pc1.create_offer().expect("offer");
    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");
    let answer = pc2.create_answer().expect("answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    let stop = AtomicBool::new(false);
    let received = Arc::new(AtomicU32::new(0));

    thread::scope(|scope| {
        // Push frames continuously so video_sink_tramp fires on a WebRTC thread
        // concurrently with the reattachment below — the exact race window.
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let bgra = vec![0x55u8; (w * h * 4) as usize];
            while !stop.load(Ordering::SeqCst) {
                video
                    .push_frame(reactor_webrtc::VideoFrame::new(&bgra, w, h))
                    .expect("push frame");
                thread::sleep(Duration::from_millis(2));
            }
        });

        // Wait for connect + the remote track to show up.
        let start = Instant::now();
        let remote = loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            if let Some(t) = s2.remote_video.lock().unwrap().clone() {
                break t;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "no remote video track within timeout"
            );
            thread::sleep(Duration::from_millis(20));
        };

        // Hammer on_video_frame from many threads while frames are flowing: each
        // (re)attachment races the native FFI call against the Rust-side store
        // on every other thread doing the same thing at the same time.
        let deadline = Instant::now() + Duration::from_secs(3);
        thread::scope(|inner| {
            for _ in 0..8 {
                let remote = &remote;
                let received = received.clone();
                inner.spawn(move || {
                    while Instant::now() < deadline {
                        let received = received.clone();
                        remote.on_frame(move |f| {
                            if !f.bgra.is_empty() {
                                received.fetch_add(1, Ordering::SeqCst);
                            }
                        });
                    }
                });
            }
        });

        stop.store(true, Ordering::SeqCst);
    });

    assert!(
        received.load(Ordering::SeqCst) > 0,
        "no sink ever observed a frame — race harness itself is broken"
    );
}
