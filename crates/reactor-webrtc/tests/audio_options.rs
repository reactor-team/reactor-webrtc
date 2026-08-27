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
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    AudioFrame, AudioTrackOptions, AudioTrackSource, IceCandidate, MediaKind, PeerConnection,
    PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RemoteTrack,
    RtcConfiguration, TransceiverDirection,
};

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const AMPLITUDE: f64 = 8_000.0;

// The two buses carry steady tones rather than DC levels: a constant offset
// does not survive the Opus round-trip, a tone does — so the received lanes
// can be told apart by their *content*, which is what "routed independently"
// actually means. The two frequencies are deliberately not harmonically
// related, so harmonic distortion of one never lands in the other's bin.
const ADM_HZ: f64 = 440.0;
const PUSH_HZ: f64 = 2_700.0;
// Head of the decoded stream to discard (100 ms): the first packets ramp up.
const SKIP_SAMPLES: usize = (RATE / 10) as usize;
// 500 ms of decoded mono audio per lane — plenty for a single-bin DFT.
const ANALYSIS_SAMPLES: usize = (RATE / 2) as usize;
// A lane must show this much more energy in its own bin than in the other's.
const DOMINANCE: f64 = 8.0;

// One received audio lane: how many frames arrived, and the decoded samples
// themselves (channel 0), so the test can check *which* bus they came from.
#[derive(Default)]
struct Lane {
    frames: AtomicU32,
    rate: AtomicU32,
    pcm: Mutex<Vec<i16>>,
}

impl Lane {
    fn record(&self, f: &AudioFrame<'_>) {
        self.frames.fetch_add(1, Ordering::SeqCst);
        self.rate.store(f.sample_rate, Ordering::SeqCst);
        let mut pcm = self.pcm.lock().unwrap();
        if pcm.len() < SKIP_SAMPLES + ANALYSIS_SAMPLES {
            pcm.extend(f.pcm.iter().step_by(f.channels.max(1) as usize));
        }
    }

    fn analysable(&self) -> bool {
        self.pcm.lock().unwrap().len() >= SKIP_SAMPLES + ANALYSIS_SAMPLES
    }
}

// Energy in a single DFT bin (Goertzel) — all that is needed to answer
// "which tone is on this lane".
fn tone_energy(pcm: &[i16], rate: u32, freq: f64) -> f64 {
    let coeff = 2.0 * (2.0 * PI * freq / rate as f64).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in pcm {
        let s0 = x as f64 / i16::MAX as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    ((s1 * s1 + s2 * s2 - coeff * s1 * s2) / pcm.len() as f64).max(0.0)
}

// The lane carries `own` Hz and not `other` Hz — a mixer that leaks push PCM
// into the shared ADM bus (or the reverse) fails here.
fn assert_lane_carries(name: &str, lane: &Lane, own: f64, other: f64) {
    let pcm = lane.pcm.lock().unwrap();
    let rate = lane.rate.load(Ordering::SeqCst);
    assert!(
        pcm.len() >= SKIP_SAMPLES + ANALYSIS_SAMPLES,
        "{name}: only {} decoded samples, need {}",
        pcm.len(),
        SKIP_SAMPLES + ANALYSIS_SAMPLES
    );
    let body = &pcm[SKIP_SAMPLES..];
    let own_energy = tone_energy(body, rate, own);
    let other_energy = tone_energy(body, rate, other);
    assert!(
        own_energy > other_energy * DOMINANCE,
        "{name}: expected only the {own:.0} Hz bus — {own:.0} Hz = {own_energy:.3e}, \
         {other:.0} Hz = {other_energy:.3e} ({rate} Hz, {} samples): the two sources cross",
        body.len()
    );
}

// A continuous `freq` tone, interleaved over CHANNELS, starting at absolute
// sample `start` so the phase carries across blocks.
fn tone(freq: f64, start: u64, frames: usize) -> Vec<i16> {
    let mut pcm = Vec::with_capacity(frames * CHANNELS as usize);
    for i in 0..frames {
        let phase = 2.0 * PI * freq * (start + i as u64) as f64 / RATE as f64;
        let sample = (phase.sin() * AMPLITUDE) as i16;
        pcm.extend(std::iter::repeat_n(sample, CHANNELS as usize));
    }
    pcm
}

#[derive(Default)]
struct Peer {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    // Received audio per track, keyed by on_track order: the transceivers
    // were added in order, so recv[0] == ADM, recv[1] == push.
    adm: Arc<Lane>,
    push: Arc<Lane>,
    recv: Mutex<Vec<RemoteTrack>>,
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
            move |track| {
                if let RemoteTrack::Audio(a) = &track {
                    let lane = match s.recv.lock().unwrap().len() {
                        0 => Some(s.adm.clone()),
                        1 => Some(s.push.clone()),
                        _ => None,
                    };
                    if let Some(lane) = lane {
                        a.on_frame(move |f| {
                            if !f.pcm.is_empty() {
                                lane.record(&f);
                            }
                        });
                    }
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
            let frames = (RATE / 100) as usize; // 10 ms blocks
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                factory.push_audio_frame(&tone(ADM_HZ, n, frames), RATE, CHANNELS);
                music
                    .push_frame(AudioFrame::new(&tone(PUSH_HZ, n, frames), RATE, CHANNELS))
                    .expect("push_frame");
                n += frames as u64;
                thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s2.adm.analysable() && s2.push.analysable();
            if done || start.elapsed() > Duration::from_secs(30) {
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
        s2.adm.frames.load(Ordering::SeqCst) > 0,
        "no ADM-sourced (mic) frames received"
    );
    assert!(
        s2.push.frames.load(Ordering::SeqCst) > 0,
        "no LocalPush (music) frames received"
    );
    // Presence is not routing: each lane must carry its own bus and only it.
    assert_lane_carries("ADM lane (mic)", &s2.adm, ADM_HZ, PUSH_HZ);
    assert_lane_carries("LocalPush lane (music)", &s2.push, PUSH_HZ, ADM_HZ);
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
