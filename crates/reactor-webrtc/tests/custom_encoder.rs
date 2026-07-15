//! Custom video encoder — verifies that the application-supplied encoder
//! callback is invoked with raw I420 frames when the sender pushes video.
//!
//! The custom encoder drops every frame (returns `None`) so no encoded bytes
//! reach the network; the test only checks that the callback fires.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test custom_encoder -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    CustomVideoEncoder, IceCandidate, MediaKind, PeerConnection, PeerConnectionFactory,
    PeerConnectionObserver, PeerConnectionState, RtcConfiguration, Track, TransceiverDirection,
};

#[derive(Default)]
struct Ice {
    q: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    recv: Mutex<Vec<Track>>,
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
        .on_connection_state_change({
            let s = ice.clone();
            move |state| {
                if state == PeerConnectionState::Connected {
                    s.connected.store(true, Ordering::SeqCst);
                }
            }
        })
        .on_track({
            let s = ice.clone();
            move |_kind, track| {
                s.recv.lock().unwrap().push(track);
            }
        });
    let pc = factory
        .create_peer_connection(config, observer)
        .expect("create peer connection");
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

#[test]
fn custom_encoder_receives_raw_frames() {
    // Count how many times the custom encoder callback fires.
    let raw_count = Arc::new(AtomicU32::new(0));

    let encoder = CustomVideoEncoder::new({
        let count = raw_count.clone();
        move |frame| {
            // Verify frame fields are sensible.
            assert_eq!(frame.width, 320);
            assert_eq!(frame.height, 240);
            assert!(!frame.y.is_empty());
            assert!(!frame.u.is_empty());
            assert!(!frame.v.is_empty());
            count.fetch_add(1, Ordering::SeqCst);
            None // drop — we don't produce encoded output
        }
    });

    // Both peers share the same factory (which uses the custom encoder).
    // pc2 is recvonly so it never calls the custom encoder callback — only
    // pc1's send path invokes it.
    let factory =
        PeerConnectionFactory::with_custom_video_encoder(encoder).expect("custom encoder factory");

    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    // Offer/answer.
    let offer = pc1.create_offer().expect("create offer");
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");
    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let mut bgra = vec![0x20u8; (w * h * 4) as usize];
            let mut t = 0u8;
            while !stop.load(Ordering::SeqCst) {
                for (i, b) in bgra.iter_mut().enumerate() {
                    *b = (i as u8).wrapping_add(t);
                }
                video.push_video_frame(&bgra, w, h);
                t = t.wrapping_add(7);
                thread::sleep(Duration::from_millis(30));
            }
        });

        let start = Instant::now();
        loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            if raw_count.load(Ordering::SeqCst) > 0 || start.elapsed() > Duration::from_secs(15) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let n = raw_count.load(Ordering::SeqCst);
    println!("custom encoder ✅ — callback invoked {n} time(s)");
    assert!(n > 0, "custom encoder callback was never called");
}
