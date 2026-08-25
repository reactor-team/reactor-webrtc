//! Encoded-frame transform over the safe API: two PeerConnections negotiate a
//! video transceiver; a **sender** transform on the sending side and a
//! **receiver** transform on the receiving side both observe the *encoded*
//! frames (codec bypass path). Verifies the app sees encoded payloads flowing
//! in both directions.
//!
//! Gated on a native libwebrtc being linked (see build.rs):
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test encoded_transform -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    FrameAction, FrameDirection, FrameTransform, IceCandidate, MediaKind, PeerConnection,
    PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration, Track,
    TransceiverDirection,
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

// GitHub Windows CI runners have no usable non-loopback interface for WebRTC
// to gather host candidates on, so ICE never connects. Skip on Windows; the
// same test runs on Linux and macOS.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn encoded_frames_flow_both_directions() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // pc1 sends video; pc2 receives it. Pre-add the paired transceivers so we
    // hold both handles (the recvonly one maps to the offer's video m-section).
    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    // Sender transform: observe egress encoded frames; forward unchanged.
    let sent = Arc::new(AtomicU32::new(0));
    let sent_key = Arc::new(AtomicU32::new(0));
    let send_tf = FrameTransform::new({
        let sent = sent.clone();
        let sent_key = sent_key.clone();
        move |f| {
            assert_eq!(f.direction, FrameDirection::Send);
            assert_eq!(f.kind, MediaKind::Video);
            assert!(f.mime_type.starts_with("video/"), "mime={}", f.mime_type);
            if !f.data.is_empty() {
                sent.fetch_add(1, Ordering::SeqCst);
                if f.is_key_frame {
                    sent_key.fetch_add(1, Ordering::SeqCst);
                }
            }
            FrameAction::Forward
        }
    });
    tx1.set_sender_transform(&send_tf)
        .expect("sender transform");

    // Receiver transform: observe ingress encoded frames; forward to the decoder.
    let recvd = Arc::new(AtomicU32::new(0));
    let recv_tf = FrameTransform::new({
        let recvd = recvd.clone();
        move |f| {
            assert_eq!(f.direction, FrameDirection::Receive);
            assert!(f.mime_type.starts_with("video/"), "mime={}", f.mime_type);
            if !f.data.is_empty() {
                recvd.fetch_add(1, Ordering::SeqCst);
            }
            FrameAction::Forward
        }
    });

    // Offer/answer.
    let offer = pc1.create_offer().expect("create offer");
    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");
    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    // Attach the receiver transform *after* negotiation to the transceiver that
    // actually receives (auto-created from the offer on pc2). This is the SFU
    // pattern: enumerate transceivers, find the video one, transform its
    // receiver. Keep the handle alive for the test's duration.
    let rx2 = pc2
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc2 video transceiver");
    rx2.set_receiver_transform(&recv_tf)
        .expect("receiver transform");

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            // A varying pattern so the encoder emits real (non-trivial) frames.
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
            let enough = sent.load(Ordering::SeqCst) > 0 && recvd.load(Ordering::SeqCst) > 0;
            if enough || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let (se, ke, re) = (
        sent.load(Ordering::SeqCst),
        sent_key.load(Ordering::SeqCst),
        recvd.load(Ordering::SeqCst),
    );
    assert!(se > 0, "sender transform saw no encoded egress frames");
    assert!(re > 0, "receiver transform saw no encoded ingress frames");
    println!(
        "encoded transform ✅ — sent {se} encoded frame(s) ({ke} key), received {re} encoded frame(s)"
    );
    // Keep transforms alive until here (they must outlive the transceivers' use).
    drop(send_tf);
    drop(recv_tf);
}
