//! Tearing down a sender while its transformed frames are still queued.
//!
//! With frame metadata negotiated, every outgoing video frame goes through a
//! frame transformer, and libwebrtc hands each transformed frame back to the
//! sender on a task queue the sender's transform delegate owns. A task posted
//! there keeps the delegate alive. Close the connection while such a task is
//! still pending, and that task ends up holding the delegate's last reference:
//! the delegate is destroyed on its own queue, deleting the queue joins the
//! thread it is running on, and on Linux the process aborts with
//! `Check failed: 0 == pthread_join(*handle_, nullptr) (0 vs. 35)`.
//!
//! This test closes many connections mid-stream, with large frames pushed as
//! fast as possible so tasks pile up on that queue. A crash aborts the test
//! binary. `TEARDOWN_ITERATIONS` and `TEARDOWN_WORKERS` scale the run.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/linux-x64-release/dist \
//!   cargo test -p reactor-webrtc --test sender_transform_teardown -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    RtcConfiguration, TransceiverDirection,
};

const W: u32 = 1280;
const H: u32 = 720;

#[derive(Default)]
struct Ice {
    q: Mutex<VecDeque<IceCandidate>>,
    recv: Mutex<Vec<reactor_webrtc::RemoteTrack>>,
}

fn make_peer(
    factory: &PeerConnectionFactory,
    config: &RtcConfiguration,
) -> (PeerConnection, Arc<Ice>) {
    let ice = Arc::new(Ice::default());
    let observer = PeerConnectionObserver::new()
        .on_ice_candidate({
            let s = ice.clone();
            move |c| s.q.lock().unwrap().push_back(c)
        })
        .on_track({
            let s = ice.clone();
            move |track| s.recv.lock().unwrap().push(track)
        });
    let pc = factory
        .create_peer_connection(config, observer)
        .expect("create pc");
    (pc, ice)
}

fn forward_ice(from: &Ice, to: &PeerConnection) {
    while let Some(c) = {
        let mut q = from.q.lock().unwrap();
        q.pop_front()
    } {
        let _ = to.add_ice_candidate(&c);
    }
}

fn negotiate(pc1: &PeerConnection, pc2: &PeerConnection) {
    let offer = pc1.create_offer().expect("offer");
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");
    let answer = pc2.create_answer().expect("answer");
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");
}

/// Noise frames: incompressible, so every encoded frame is large and the
/// sender spends real time packetizing it on the transform queue.
fn noise_frames(count: usize) -> Vec<Vec<u8>> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..count)
        .map(|_| {
            (0..(W * H * 4))
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect()
        })
        .collect()
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One connection: stream until frames arrive, then close the sender mid-stream.
/// Returns whether the receiver saw frames before the close.
fn stream_then_close(factory: &PeerConnectionFactory, frames: &[Vec<u8>]) -> bool {
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(factory, &config);
    let (pc2, s2) = make_peer(factory, &config);

    let tx = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory.create_video_track("teardown").expect("video track");
    tx.set_track(&video).expect("set track");
    negotiate(&pc1, &pc2);
    assert!(
        pc1.frame_metadata_gate().is_open(),
        "frame metadata must be negotiated so the sender transform is installed"
    );
    let _ = pc1.set_bitrate(Some(8_000_000), Some(20_000_000), Some(50_000_000));

    let received = Arc::new(AtomicUsize::new(0));
    let stop = AtomicBool::new(false);
    let mut pc1 = Some(pc1);

    let saw_frames = thread::scope(|scope| {
        scope.spawn(|| {
            let mut i = 0usize;
            while !stop.load(Ordering::SeqCst) {
                let frame = reactor_webrtc::VideoFrame::new(&frames[i % frames.len()], W, H);
                let _ = video.push_frame_with_metadata(frame, b"teardown");
                i += 1;
                thread::sleep(Duration::from_millis(5));
            }
        });

        let start = Instant::now();
        let mut sink_set = false;
        let pc1_ref = pc1.as_ref().unwrap();
        let saw = loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, pc1_ref);
            if !sink_set {
                if let Some(remote) = s2.recv.lock().unwrap().iter().find_map(|t| t.as_video()) {
                    let n = received.clone();
                    remote.on_frame(move |_| {
                        n.fetch_add(1, Ordering::SeqCst);
                    });
                    sink_set = true;
                }
            }
            if received.load(Ordering::SeqCst) >= 5 {
                break true;
            }
            if start.elapsed() > Duration::from_secs(15) {
                break false;
            }
            thread::sleep(Duration::from_millis(10));
        };

        // Close the sender while frames are still being pushed and transformed.
        drop(pc1.take());
        thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::SeqCst);
        saw
    });

    drop(pc2);
    saw_frames
}

#[test]
fn closing_a_sender_with_transformed_frames_in_flight_does_not_abort() {
    let iterations = env_usize("TEARDOWN_ITERATIONS", 20);
    let workers = env_usize("TEARDOWN_WORKERS", 2);

    // One factory for the process, the way the runtime uses it.
    let factory = Arc::new(PeerConnectionFactory::builder().build().expect("factory"));
    let frames = Arc::new(noise_frames(4));
    let streamed = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let (factory, frames, streamed, done) = (
                factory.clone(),
                frames.clone(),
                streamed.clone(),
                done.clone(),
            );
            thread::spawn(move || {
                while done.fetch_add(1, Ordering::SeqCst) < iterations {
                    if stream_then_close(&factory, &frames) {
                        streamed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }

    let streamed = streamed.load(Ordering::SeqCst);
    println!("closed {iterations} connections mid-stream ({streamed} had frames flowing)");
    assert!(
        streamed * 2 >= iterations,
        "too few connections streamed before the close ({streamed}/{iterations}); \
         the teardown race was not exercised"
    );
}
