//! Encoder feedback: rate-control (BWE) updates surface on custom-encoded
//! tracks — via `EncodedVideoTrack::on_encoder_feedback` and via
//! `VideoTrack::on_encoder_feedback` for inline encoders. Keyframe requests
//! ride `RawVideoFrame::request_key_frame` on the inline path and the feedback
//! listener on the pre-encoded path.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test encoder_feedback -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    EncodedVideoFrame, EncoderFeedback, IceCandidate, LocalVideoTrack, MediaKind, PeerConnection,
    PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, PreEncodedOptions,
    RtcConfiguration, TrackVideoEncoder, TransceiverDirection, VideoTrackOptions,
};

#[derive(Default)]
struct Peer {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
}

fn make_peer(
    factory: &PeerConnectionFactory,
    config: &RtcConfiguration,
) -> (PeerConnection, Arc<Peer>) {
    let s = Arc::new(Peer::default());
    let obs = PeerConnectionObserver::new()
        .on_ice_candidate({
            let s = s.clone();
            move |c| s.ice.lock().unwrap().push_back(c)
        })
        .on_connection_state_change({
            let s = s.clone();
            move |state| {
                if state == PeerConnectionState::Connected {
                    s.connected.store(true, Ordering::SeqCst);
                }
            }
        });
    let pc = factory
        .create_peer_connection(config, obs)
        .expect("peer connection");
    (pc, s)
}

fn trickle(from: &Peer, to: &PeerConnection) {
    while let Some(c) = from.ice.lock().unwrap().pop_front() {
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

#[test]
fn rate_updates_surface_on_a_pre_encoded_track() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let rate_updates = Arc::new(AtomicU32::new(0));
    let keyframes = Arc::new(AtomicU32::new(0));

    let encoded = {
        let mut options = VideoTrackOptions::default();
        options.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(
            320, 240,
        )));
        match factory
            .create_video_track_with_options("enc", options)
            .expect("encoded track")
        {
            LocalVideoTrack::Encoded(t) => t,
            LocalVideoTrack::Raw(_) => panic!("expected a pre-encoded track"),
        }
    };
    encoded.on_encoder_feedback({
        let rate_updates = rate_updates.clone();
        let keyframes = keyframes.clone();
        move |fb| match fb {
            EncoderFeedback::RateUpdate { bitrate_bps, .. } => {
                assert!(
                    bitrate_bps > 0,
                    "rate updates must carry a positive bitrate"
                );
                rate_updates.fetch_add(1, Ordering::SeqCst);
            }
            EncoderFeedback::KeyFrameRequest => {
                keyframes.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    });

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("tx");
    tx1.set_track(encoded.track()).expect("set track");
    negotiate(&pc1, &pc2);

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut i = 0u32;
            while !stop.load(Ordering::SeqCst) {
                encoded
                    .push_frame(EncodedVideoFrame {
                        data: vec![0xAA; 64],
                        is_key_frame: i % 30 == 0,
                        width: 0,
                        height: 0,
                        rtp_timestamp: 0,
                    })
                    .expect("push frame");
                i += 1;
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);
            if done {
                // A deterministic rate-change nudge: set_bitrate produces a
                // fresh RateControlParameters → SetRates → our feedback.
                pc1.set_bitrate(Some(50_000), Some(200_000), Some(1_000_000))
                    .expect("set_bitrate");
            }
            let got = !start
                .elapsed()
                .gt(&Duration::from_secs(2))
                .then_some(0)
                .is_none()
                && rate_updates.load(Ordering::SeqCst) > 0;
            if got || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    assert!(
        rate_updates.load(Ordering::SeqCst) > 0,
        "no rate-update feedback arrived on the pre-encoded track"
    );
    println!(
        "encoder_feedback ✅ — {} rate update(s), {} keyframe request(s) seen",
        rate_updates.load(Ordering::SeqCst),
        keyframes.load(Ordering::SeqCst)
    );
}

#[test]
fn rate_updates_surface_on_an_inline_encoder_track() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let rate_updates = Arc::new(AtomicU32::new(0));
    let raw_keyframes = Arc::new(AtomicU32::new(0));

    let inline = {
        let mut options = VideoTrackOptions::default();
        options.encoder = Some(TrackVideoEncoder::Inline(Box::new({
            let raw_keyframes = raw_keyframes.clone();
            move |raw| {
                if raw.request_key_frame {
                    raw_keyframes.fetch_add(1, Ordering::SeqCst);
                }
                None // drop — the pipeline drives the callback; no bytes needed
            }
        })));
        match factory
            .create_video_track_with_options("inline", options)
            .expect("inline track")
        {
            LocalVideoTrack::Raw(t) => t,
            LocalVideoTrack::Encoded(_) => panic!("inline track must be raw"),
        }
    };
    inline
        .on_encoder_feedback({
            let rate_updates = rate_updates.clone();
            move |fb| {
                let EncoderFeedback::RateUpdate { .. } = fb else {
                    return;
                };
                rate_updates.fetch_add(1, Ordering::SeqCst);
            }
        })
        .expect("on_encoder_feedback on an inline track");

    // A plain raw track must not accept a feedback listener: its encoder
    // adapts internally.
    let plain = factory.create_video_track("plain").expect("plain track");
    match plain.on_encoder_feedback(|_| {}) {
        Ok(_) => panic!("feedback listener must not register on a builtin-encoder track"),
        Err(e) => assert!(
            e.to_string().contains("Inline"),
            "unexpected error message: {e}"
        ),
    }

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("tx");
    tx1.set_track(&inline).expect("set track");
    negotiate(&pc1, &pc2);

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let bgra = vec![128u8; (w * h * 4) as usize];
            while !stop.load(Ordering::SeqCst) {
                inline
                    .push_frame(reactor_webrtc::VideoFrame::new(&bgra, w, h))
                    .expect("push frame");
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);
            if done {
                pc1.set_bitrate(Some(50_000), Some(200_000), Some(1_000_000))
                    .expect("set_bitrate");
            }
            if rate_updates.load(Ordering::SeqCst) > 0 || start.elapsed() > Duration::from_secs(20)
            {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    assert!(
        rate_updates.load(Ordering::SeqCst) > 0,
        "no rate-update feedback arrived on the inline track"
    );
    println!(
        "inline feedback ✅ — {} rate update(s), {} raw keyframe flag(s)",
        rate_updates.load(Ordering::SeqCst),
        raw_keyframes.load(Ordering::SeqCst)
    );
}
