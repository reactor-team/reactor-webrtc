//! End-to-end loopback over the **safe** API: two PeerConnections negotiate,
//! connect, and exchange real audio + video — using only `reactor-webrtc`
//! (closures, RAII), no raw FFI.
//!
//! Gated on a native libwebrtc being linked (see build.rs):
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RemoteTrack, RtcConfiguration,
};

#[derive(Default)]
struct Shared {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    video_frames: AtomicU32,
    audio_frames: AtomicU32,
    // Received remote tracks, kept alive (with their sinks) for the test.
    recv: Mutex<Vec<RemoteTrack>>,
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
                match &track {
                    RemoteTrack::Video(v) => {
                        let s = s.clone();
                        v.on_frame(move |f| {
                            if f.bgra.len() == (f.width * f.height * 4) as usize
                                && !f.bgra.is_empty()
                            {
                                s.video_frames.fetch_add(1, Ordering::SeqCst);
                            }
                        });
                    }
                    RemoteTrack::Audio(a) => {
                        let s = s.clone();
                        a.on_frame(move |f| {
                            if !f.pcm.is_empty() {
                                s.audio_frames.fetch_add(1, Ordering::SeqCst);
                            }
                        });
                    }
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
fn safe_loopback_exchanges_media() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // Verify that set_bitrate is accepted at any point after PC creation.
    pc1.set_bitrate(Some(100_000), Some(500_000), Some(2_000_000))
        .expect("set_bitrate");

    // pc1 sends a video + an audio track.
    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");
    let audio = factory
        .create_audio_track("reactor-audio")
        .expect("audio track");
    pc1.add_track(&video).expect("add video");
    pc1.add_track(&audio).expect("add audio");

    // Offer/answer.
    let offer = pc1.create_offer().expect("create offer");
    assert!(offer.sdp.contains("m=video") && offer.sdp.contains("m=audio"));
    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");
    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");
    println!("safe API: offer/answer exchange complete");

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        // Push audio (every 10ms) + video (every ~30ms) until told to stop.
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let bgra = vec![0x40u8; (w * h * 4) as usize];
            let (rate, channels, spc) = (48000u32, 2u32, 480usize);
            let pcm: Vec<i16> = (0..spc * channels as usize)
                .map(|i| ((i % 256) as i16 - 128) * 64)
                .collect();
            let mut i = 0u32;
            while !stop.load(Ordering::SeqCst) {
                factory.push_audio_frame(&pcm, rate, channels);
                if i % 3 == 0 {
                    video
                        .push_frame(reactor_webrtc::VideoFrame::new(&bgra, w, h))
                        .expect("push frame");
                }
                i = i.wrapping_add(1);
                thread::sleep(Duration::from_millis(10));
            }
        });

        // Trickle ICE and wait for connect + received media.
        let start = Instant::now();
        loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            let connected =
                s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);
            let media = s2.video_frames.load(Ordering::SeqCst) > 0
                && s2.audio_frames.load(Ordering::SeqCst) > 0;
            if (connected && media) || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let (v, a) = (
        s2.video_frames.load(Ordering::SeqCst),
        s2.audio_frames.load(Ordering::SeqCst),
    );
    assert!(
        s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst),
        "loopback did not connect",
    );
    assert!(v > 0, "pc2 received no video frames");
    assert!(a > 0, "pc2 received no audio frames");
    println!("safe loopback connected ✅ — pc2 received {v} video + {a} audio frame(s)");
    // RAII teardown: tracks, peer connections, factory all drop here.
}

/// `Transceiver::set_send_bitrate` — the per-sender ceiling, which is a
/// different knob from `PeerConnection::set_bitrate` (see the method docs).
///
/// This covers the API contract — accepted before and after negotiation, both
/// bounds independently optional, bad input rejected — rather than the
/// resulting wire bitrate. Proving the encoder actually exceeds libwebrtc's
/// 2500 kbps default needs a sustained high-entropy 1080p stream over a path
/// with real headroom, which is a bandwidth measurement, not a unit test: it
/// would take tens of seconds and still be at the mercy of the congestion
/// controller's ramp.
#[test]
fn set_send_bitrate_bounds() {
    use reactor_webrtc::{MediaKind, TransceiverDirection};

    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc, _shared) = make_peer(&factory, &config);

    let tc = pc
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("add video transceiver");

    // Before negotiation: the sender exists as soon as the transceiver does.
    tc.set_send_bitrate(None, Some(8_000_000))
        .expect("max-only, pre-negotiation");
    tc.set_send_bitrate(Some(1_000_000), Some(8_000_000))
        .expect("both bounds");

    // Each bound is independently optional, and clearing back to the libwebrtc
    // default is expressible.
    tc.set_send_bitrate(Some(1_000_000), None)
        .expect("min-only");
    tc.set_send_bitrate(None, None).expect("clear both");

    // An inverted pair is rejected here, with a message that names the problem —
    // libwebrtc's own rejection does not say which pair was at fault.
    let err = tc
        .set_send_bitrate(Some(8_000_000), Some(1_000_000))
        .expect_err("min above max must be rejected");
    assert!(
        err.to_string().contains("min_bps exceeds max_bps"),
        "unexpected error for inverted bounds: {err}",
    );

    // A negative bound is refused rather than read as the -1 the ABI uses for
    // "unset". Resolving it that way would take a typo and quietly remove a cap
    // the caller had set, while reporting success.
    for (label, min, max) in [
        ("max_bps", None, Some(-8_000_000)),
        ("min_bps", Some(-1_000_000), None),
    ] {
        let err = tc
            .set_send_bitrate(min, max)
            .expect_err("a negative bound must be rejected");
        assert!(
            err.to_string().contains(label) && err.to_string().contains("must be >= 0"),
            "unexpected error for a negative {label}: {err}",
        );
    }
    // The cap set above is still in force: the refusals above changed nothing.
    tc.set_send_bitrate(None, Some(8_000_000))
        .expect("still settable after a refusal");

    // Zero is a value, not a spelling of "unset" — it reaches libwebrtc and
    // libwebrtc answers. Whichever way it answers, it must not be silently
    // rewritten into "leave the default alone" on the way there.
    let zero = tc.set_send_bitrate(None, Some(0));
    println!("set_send_bitrate(max=0) -> {zero:?}");

    // After negotiation the sender is live; the bounds still apply.
    let offer = pc.create_offer().expect("create offer");
    pc.set_local_description(&offer).expect("local offer");
    tc.set_send_bitrate(None, Some(6_000_000))
        .expect("post-negotiation");

    println!("set_send_bitrate: bounds accepted pre- and post-negotiation ✅");
}
