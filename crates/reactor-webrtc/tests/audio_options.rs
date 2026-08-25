//! Per-track audio options: an ADM-sourced track and an independent
//! LocalPush track coexist on one factory with independently routed audio
//! (the "mic + music" shape), and the per-source processing constraints are
//! accepted.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test audio_options -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    AudioTrackOptions, AudioTrackSource, IceCandidate, MediaKind, PeerConnection,
    PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration, Track,
    TransceiverDirection,
};

#[derive(Default)]
struct Peer {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    // Frames decoded per received track, keyed by on_track order: the
    // transceivers were added in order, so recv[0] == ADM, recv[1] == push.
    adm_frames: AtomicU32,
    push_frames: AtomicU32,
    recv: Mutex<Vec<Track>>,
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
            move |kind, track| {
                if kind == MediaKind::Audio {
                    let idx = s.recv.lock().unwrap().len();
                    let s = s.clone();
                    track.on_audio_frame(move |f| {
                        if f.pcm.is_empty() {
                            return;
                        }
                        match idx {
                            0 => s.adm_frames.fetch_add(1, Ordering::SeqCst),
                            1 => s.push_frames.fetch_add(1, Ordering::SeqCst),
                            _ => 0,
                        };
                    });
                }
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

// The mic + music shape: a track fed by the factory's (synthetic) ADM next
// to an independent LocalPush track — the two streams must arrive at the
// peer without crossing.
#[test]
fn adm_and_local_push_tracks_route_independently_() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    let mic = factory.create_audio_track("mic").expect("mic track");
    let music = factory
        .create_audio_track_with_options("music", {
            let mut options = AudioTrackOptions::default();
            options.source = AudioTrackSource::LocalPush;
            options
        })
        .expect("music track");

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);
    pc1.add_track(&mic).expect("add mic");
    pc1.add_track(&music).expect("add music");

    negotiate(&pc1, &pc2);

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let rate = 48_000u32;
            let channels = 2u32;
            let block = (rate / 100) as usize * channels as usize; // 10ms @ 48kHz stereo
            let adm_pcm = vec![4_000i16; block];
            let push_pcm = vec![-8_000i16; block];
            while !stop.load(Ordering::SeqCst) {
                factory.push_audio_frame(&adm_pcm, rate, channels);
                music.push_pcm(&push_pcm, rate, channels).expect("push_pcm");
                thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s2.adm_frames.load(Ordering::SeqCst) > 0
                && s2.push_frames.load(Ordering::SeqCst) > 0;
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
    assert!(
        s2.adm_frames.load(Ordering::SeqCst) > 0,
        "no ADM-sourced (mic) frames received"
    );
    assert!(
        s2.push_frames.load(Ordering::SeqCst) > 0,
        "no LocalPush (music) frames received"
    );
}

// The 4 per-source processing constraints in both positions are accepted —
// creation + negotiation smoke (room-dependent DSP behaviour itself is not
// observable on a synthetic headless ADM).
#[test]
fn processing_constraints_are_accepted() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    for (name, ec, ns, agc, hpf) in [
        ("all-on", Some(true), Some(true), Some(true), Some(true)),
        (
            "all-off",
            Some(false),
            Some(false),
            Some(false),
            Some(false),
        ),
        ("mixed", Some(true), Some(false), Some(false), Some(true)),
    ] {
        let options = {
            let mut o = AudioTrackOptions::default();
            o.source = AudioTrackSource::Adm;
            o.echo_cancellation = ec;
            o.noise_suppression = ns;
            o.auto_gain_control = agc;
            o.high_pass_filter = hpf;
            o
        };
        let track = factory
            .create_audio_track_with_options(name, options)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let pc = factory
            .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
            .expect("pc");
        let tx = pc
            .add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly)
            .expect("tx");
        tx.set_track(&track).expect("set track");
        pc.create_offer().expect("offer");
    }
}
