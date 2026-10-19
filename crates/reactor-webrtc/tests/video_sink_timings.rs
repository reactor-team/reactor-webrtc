//! `VideoTrack::sink_timings` over a real loopback: zero before a sink is
//! attached, adding up the frames the closure received, starting again from
//! zero on a new sink, and readable from inside the closure.
//!
//! Gated on a native libwebrtc being linked (see build.rs).
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RemoteTrack, RtcConfiguration, VideoFrame, VideoSinkTimings, VideoTrack,
};

#[derive(Default)]
struct Shared {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    video: Mutex<Option<VideoTrack>>,
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
                    *s.video.lock().unwrap() = Some(v);
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

/// Poll `done` every 20 ms for up to `limit`; whether it came true.
fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    done()
}

#[test]
fn sink_timings_add_up_the_frames_of_the_sink_attached_last() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");
    assert_eq!(
        video.sink_timings(),
        VideoSinkTimings::default(),
        "zero before a sink is attached"
    );
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

    let pushing = AtomicBool::new(true);
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let bgra = vec![0x40u8; (w * h * 4) as usize];
            while !stop.load(Ordering::SeqCst) {
                if pushing.load(Ordering::SeqCst) {
                    video
                        .push_frame(VideoFrame::new(&bgra, w, h))
                        .expect("push frame");
                }
                thread::sleep(Duration::from_millis(30));
            }
        });

        let connected = wait_for(Duration::from_secs(20), || {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            s1.connected.load(Ordering::SeqCst)
                && s2.connected.load(Ordering::SeqCst)
                && s2.video.lock().unwrap().is_some()
        });
        assert!(connected, "loopback did not connect");
        let remote = Arc::new(s2.video.lock().unwrap().take().expect("remote video"));
        assert_eq!(
            remote.sink_timings(),
            VideoSinkTimings::default(),
            "zero before a sink is attached"
        );

        // A first sink, timed per frame, that also reads the totals from inside
        // the closure, as a caller logging a mean every N frames would.
        let first_frames = Arc::new(AtomicU64::new(0));
        let first_convert = Arc::new(AtomicU64::new(0));
        let read_inside = Arc::new(AtomicU64::new(0));
        let reader = Arc::new(Mutex::new(None::<Arc<VideoTrack>>));
        remote.on_frame_timed({
            let (frames, convert) = (first_frames.clone(), first_convert.clone());
            let (read_inside, reader) = (read_inside.clone(), reader.clone());
            move |_, took| {
                frames.fetch_add(1, Ordering::SeqCst);
                convert.fetch_add(took.as_nanos() as u64, Ordering::SeqCst);
                if let Some(track) = reader.lock().unwrap().as_ref() {
                    read_inside.store(track.sink_timings().frames, Ordering::SeqCst);
                }
            }
        });
        assert!(
            wait_for(Duration::from_secs(10), || {
                first_frames.load(Ordering::SeqCst) >= 10
            }),
            "the first sink received fewer than 10 frames"
        );

        // Re-attach from this thread while the closure keeps reading the totals.
        // Attaching holds the sink's lock while it waits on the thread that runs
        // the closure, so totals read through that lock could deadlock here.
        *reader.lock().unwrap() = Some(Arc::clone(&remote));
        assert!(wait_for(Duration::from_secs(5), || {
            read_inside.load(Ordering::SeqCst) > 0
        }));
        let second_frames = Arc::new(AtomicU64::new(0));
        remote.on_frame({
            let frames = second_frames.clone();
            move |_| {
                frames.fetch_add(1, Ordering::SeqCst);
            }
        });
        let first = (
            first_frames.load(Ordering::SeqCst),
            first_convert.load(Ordering::SeqCst),
        );
        assert!(
            remote.sink_timings().frames <= second_frames.load(Ordering::SeqCst) + 1,
            "a new sink starts again from zero"
        );
        assert!(
            wait_for(Duration::from_secs(10), || {
                second_frames.load(Ordering::SeqCst) >= 10
            }),
            "the second sink received fewer than 10 frames"
        );

        // Once no frame is in flight, the totals are exactly the second sink's.
        pushing.store(false, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(500));
        let timings = remote.sink_timings();
        assert_eq!(timings.frames, second_frames.load(Ordering::SeqCst));
        assert!(timings.convert > Duration::ZERO);

        // The first sink was handed each frame's conversion time.
        assert!(first.0 >= 10 && first.1 > 0, "{first:?}");
        stop.store(true, Ordering::SeqCst);
    });
}
