//! Stress test for the `run_sdp`/`run_complete`/`run_stats` async-op bridge in
//! `peer_connection.rs`.
//!
//! Each bridge hands libwebrtc's signaling thread a pointer to a boxed
//! `SyncSender`, waits on the paired `Receiver`, and — in the buggy version —
//! frees that box the instant `recv_timeout` returns a value. `recv_timeout`
//! can return as soon as the value becomes visible, which is *before* the
//! sending thread's `try_send` has necessarily finished unwinding out of its
//! own internal `notify()`. That thread then dereferences freed memory: a
//! flaky, timing-dependent segfault (`EXC_BAD_ACCESS`/`SIGSEGV`) inside
//! `std::sync::mpmc::waker::SyncWaker::notify`.
//!
//! This can't be forced deterministically without patching `std`'s internals.
//! Empirically, hammering `create_offer` alone with no media flowing, even at
//! very high concurrency/volume, was not enough to reproduce it — the
//! original crash happened inside a real loopback connection with audio +
//! video actually flowing (more libwebrtc subsystems active, more threads
//! under real load). This test repeatedly runs a full connect-with-media
//! cycle instead of a bare `create_offer` loop, and is meant to be run as
//! several concurrent *processes* (see the repo's segfault investigation
//! notes) rather than relying on in-process thread concurrency alone — the
//! original crash reproduced under cross-process CPU contention (parallel
//! pytest workers), not multiple threads sharing one factory.
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RtcConfiguration,
};

#[derive(Default)]
struct Shared {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    video_frames: AtomicU32,
    recv: Mutex<Vec<reactor_webrtc::Track>>,
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
            move |kind, mut track| {
                if kind == MediaKind::Video {
                    let s = s.clone();
                    track.on_video_frame(move |f| {
                        if !f.bgra.is_empty() {
                            s.video_frames.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }
                s.recv.lock().unwrap().push(track);
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
fn sdp_bridge_survives_repeated_connects_with_media() {
    let factory = PeerConnectionFactory::new().expect("factory");
    let config = RtcConfiguration::default();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut iterations = 0u32;

    while Instant::now() < deadline {
        let (pc1, s1) = make_peer(&factory, &config);
        let (pc2, s2) = make_peer(&factory, &config);

        let video = factory
            .create_video_track("race-video")
            .expect("video track");
        pc1.add_track(&video).expect("add video");

        let offer = pc1.create_offer().expect("create offer");
        pc1.set_local_description(&offer).expect("pc1 local offer");
        pc2.set_remote_description(&offer)
            .expect("pc2 remote offer");
        let answer = pc2.create_answer().expect("create answer");
        pc2.set_local_description(&answer)
            .expect("pc2 local answer");
        pc1.set_remote_description(&answer)
            .expect("pc1 remote answer");

        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                let (w, h) = (320u32, 240u32);
                let bgra = vec![0x40u8; (w * h * 4) as usize];
                while !stop.load(Ordering::SeqCst) {
                    video.push_video_frame(&bgra, w, h);
                    thread::sleep(Duration::from_millis(15));
                }
            });

            let start = Instant::now();
            loop {
                forward_ice(&s1, &pc2);
                forward_ice(&s2, &pc1);
                let _ = pc1.get_stats().expect("get_stats");
                let connected = s2.connected.load(Ordering::SeqCst);
                let media = s2.video_frames.load(Ordering::SeqCst) > 0;
                if (connected && media) || start.elapsed() > Duration::from_secs(10) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::SeqCst);
        });

        iterations += 1;
    }

    println!("sdp_bridge_survives_repeated_connects_with_media: {iterations} connect cycles completed with no crash");
    assert!(iterations > 0, "no iterations completed — setup is broken");
}
