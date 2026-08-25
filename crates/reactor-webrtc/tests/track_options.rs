//! Per-track video options: mixed raw + pre-encoded + inline tracks on one
//! factory, plus the SDP codec-advertisement gate (H264/H265 only claimed
//! while a custom slot exists).
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test track_options -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    EncodedVideoFrame, IceCandidate, LocalVideoTrack, MediaKind, PeerConnection,
    PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, PreEncodedOptions,
    RtcConfiguration, TrackVideoEncoder, TransceiverDirection, VideoTrackOptions,
};

#[derive(Default)]
struct Peer {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    video_frames: AtomicU32,
    // Received remote tracks, kept alive (with their sinks) for the test.
    recv: Mutex<Vec<reactor_webrtc::Track>>,
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
        })
        .on_track({
            let s = s.clone();
            move |_kind, track| {
                track.on_video_frame({
                    let s = s.clone();
                    move |_f| {
                        s.video_frames.fetch_add(1, Ordering::SeqCst);
                    }
                });
                s.recv.lock().unwrap().push(track); // keep the sink alive
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

fn offer_sdp(factory: &PeerConnectionFactory) -> String {
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("pc");
    let tx = pc
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("tx");
    let track = factory.create_video_track("probe").expect("track");
    tx.set_track(&track).expect("set track");
    pc.create_offer().expect("offer").sdp
}

#[test]
fn custom_codecs_are_advertised_only_with_custom_slots() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");

    // Registry empty (only builtin slots so far): no H265 anywhere; H264 only
    // where a real backend exists (Apple VideoToolbox).
    let plain = offer_sdp(&factory).to_lowercase();
    assert!(
        !plain.contains("h265"),
        "H265 claimed without custom slots: {plain}"
    );
    #[cfg(not(target_vendor = "apple"))]
    assert!(
        !plain.contains("h264"),
        "H264 claimed without a backend or custom slots: {plain}"
    );

    // Add a pre-encoded track → the registry gains a custom slot: H264+H265
    // must be claimable now (the track delivers its own bitstream).
    let mut options = VideoTrackOptions::default();
    options.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(
        320, 240,
    )));
    factory
        .create_video_track_with_options("enc", options)
        .expect("encoded track");
    let with_custom = offer_sdp(&factory).to_lowercase();
    assert!(
        with_custom.contains("h265"),
        "H265 should be claimed with a custom slot: {with_custom}"
    );
    assert!(
        with_custom.contains("h264"),
        "H264 should be claimed with a custom slot: {with_custom}"
    );
}

// One factory carrying a raw track (builtin encode), a pre-encoded track and
// an inline-encoder track at once — the positional slot routing must keep the
// three pipelines apart.
#[test]
fn raw_and_pre_encoded_and_inline_tracks_coexist() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // Creation order == transceiver order (positional slot assignment).
    let raw = factory.create_video_track("camera").expect("raw track");
    let encoded = {
        let mut options = VideoTrackOptions::default();
        options.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(
            320, 240,
        )));
        match factory
            .create_video_track_with_options("screen", options)
            .expect("encoded track")
        {
            LocalVideoTrack::Encoded(t) => t,
            LocalVideoTrack::Raw(_) => panic!("expected a pre-encoded track"),
        }
    };
    let inline = {
        let mut options = VideoTrackOptions::default();
        // Inline encoder that re-encodes nothing: drop every frame; the point
        // of including it here is that its pipeline stays separate, which
        // would break if the inline slot were miswired (its callback would
        // fire on pc1's raw frames of the WRONG track).
        options.encoder = Some(TrackVideoEncoder::Inline(Box::new(|_raw| None)));
        match factory
            .create_video_track_with_options("inline", options)
            .expect("inline track")
        {
            LocalVideoTrack::Raw(t) => t,
            LocalVideoTrack::Encoded(_) => panic!("inline track must be raw"),
        }
    };
    for t in [&raw, encoded.track(), &inline] {
        let tx = pc1
            .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
            .expect("tx");
        tx.set_track(t).expect("set track");
    }

    negotiate(&pc1, &pc2);

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let bgra = vec![128u8; (w * h * 4) as usize];
            let mut i = 0u32;
            while !stop.load(Ordering::SeqCst) {
                raw.push_video_frame(&bgra, w, h);
                inline.push_video_frame(&bgra, w, h);
                encoded.push_encoded_frame(EncodedVideoFrame {
                    data: vec![0xAA; 64],
                    is_key_frame: i % 30 == 0,
                    width: 0,
                    height: 0,
                    rtp_timestamp: 0,
                });
                i += 1;
                thread::sleep(Duration::from_millis(33));
            }
        });
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s2.video_frames.load(Ordering::SeqCst) > 0;
            if done || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    assert!(
        s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst),
        "loopback did not connect"
    );
    // The raw (builtin-encoded) track must produce decodable frames on pc2;
    // the stub pre-encoded bytes are garbage to any real decoder, so only the
    // raw track's output is asserted here.
    assert!(
        s2.video_frames.load(Ordering::SeqCst) > 0,
        "pc2 received no video frames from the raw track"
    );
}
