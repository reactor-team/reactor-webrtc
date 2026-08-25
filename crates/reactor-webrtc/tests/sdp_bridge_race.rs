//! Stress test / manual reproducer for the `run_sdp`/`run_complete`/
//! `run_stats` async-op bridge in `peer_connection.rs`.
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
//! This can't be forced deterministically without patching `std`'s internals,
//! and empirically a single process — even hammering `create_offer` with no
//! media flowing across dozens of threads — was not enough to reproduce it.
//! What did: several concurrent *processes* (cross-process CPU contention,
//! the way parallel pytest workers on CI ended up producing it), each
//! running a real loopback connection with audio + video actually flowing.
//! `#[ignore]`d so it doesn't cost every `cargo test` run on every platform
//! for a reproduction path that single-process runs can't exercise anyway;
//! run it manually as the reproducer it's meant to be:
//!
//! ```sh
//! cargo build --release --tests -p reactor-webrtc
//! bin=$(find target/release/deps -maxdepth 1 -name 'sdp_bridge_race-*' -perm +111)
//! for i in 1 2 3 4 5 6; do "$bin" --ignored --nocapture & done; wait
//! ```
//!
//! Against the unfixed bridge this reliably kills all 6 processes with
//! SIGSEGV (exit 139) within the first cycle or two; against the fix, all 6
//! complete ~1700 connect cycles apiece in the 45s budget below with none.
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
            move |kind, track| {
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
#[ignore = "reproducer meant to be run as several concurrent processes — see module docs"]
fn sdp_bridge_survives_repeated_connects_with_media() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut iterations = 0u32;
    let mut connected_iterations = 0u32;
    const PER_CYCLE_TIMEOUT: Duration = Duration::from_secs(10);

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
        let mut this_cycle_connected = false;
        thread::scope(|scope| {
            scope.spawn(|| {
                // Its own deadline, independent of `stop`: if the main leg of
                // this scope panics (e.g. a `get_stats` timeout under the
                // heavy contention this test is designed for) before it can
                // set `stop`, this thread must still exit on its own —
                // otherwise `thread::scope` blocks joining it forever and a
                // test failure turns into an indefinite CI hang.
                let pump_deadline = Instant::now() + PER_CYCLE_TIMEOUT + Duration::from_secs(1);
                let (w, h) = (320u32, 240u32);
                let bgra = vec![0x40u8; (w * h * 4) as usize];
                while !stop.load(Ordering::SeqCst) && Instant::now() < pump_deadline {
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
                if connected && media {
                    this_cycle_connected = true;
                    break;
                }
                if start.elapsed() > PER_CYCLE_TIMEOUT {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::SeqCst);
        });

        iterations += 1;
        if this_cycle_connected {
            connected_iterations += 1;
        }
    }

    println!(
        "sdp_bridge_survives_repeated_connects_with_media: \
         {connected_iterations}/{iterations} connect cycles succeeded with no crash"
    );
    // A UAF in the bridge kills the process outright (this assertion never
    // gets to run) — this guards against the other way the test could go
    // quietly wrong: the bridge silently breaking so nothing ever connects.
    assert_eq!(
        connected_iterations, iterations,
        "at least one connect cycle failed to connect+receive media within {PER_CYCLE_TIMEOUT:?}"
    );
}
