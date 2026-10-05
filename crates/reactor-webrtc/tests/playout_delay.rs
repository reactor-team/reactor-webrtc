//! End-to-end tests for the video buffering controls: the playout delay a
//! factory sends or forces on receive, and a transceiver's jitter buffer
//! minimum delay. Each is checked where it lands — the receiver's
//! `jitterBufferDelay` per emitted frame, i.e. how long a frame waited between
//! its first packet arriving and it leaving for the decoder.
//!
//! Requires a native libwebrtc (see build.rs):
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test playout_delay -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, PlayoutDelay, RtcConfiguration, StreamKind, TransceiverDirection,
    VideoFrame,
};

#[derive(Default)]
struct Ice {
    q: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
}

fn make_peer(factory: &PeerConnectionFactory) -> (PeerConnection, Arc<Ice>) {
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
        });
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), observer)
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

const W: u32 = 320;
const H: u32 = 240;
/// Frames the receiver must have emitted before its average is read.
const FRAMES: u64 = 60;

/// Stream video from a `sender` factory to a `receiver` factory and return the
/// receiver's average jitter buffer delay and average target delay, per frame.
///
/// `before_negotiation` runs on the receiving transceiver before any
/// description is applied — before any stream exists — which is where a
/// setting has to land to hold from the first frame. The receiver makes the
/// offer so that this transceiver is the one carrying the stream: JSEP never
/// reuses an `add_transceiver` transceiver for a remote offer's m-section.
fn average_receive_delays(
    sender: &PeerConnectionFactory,
    receiver: &PeerConnectionFactory,
    before_negotiation: impl FnOnce(&reactor_webrtc::Transceiver),
) -> (Duration, Duration) {
    let (pc1, s1) = make_peer(sender);
    let (pc2, s2) = make_peer(receiver);

    let rx2 = pc2
        .add_transceiver(MediaKind::Video, TransceiverDirection::RecvOnly)
        .expect("recv transceiver");
    before_negotiation(&rx2);

    let offer = pc2.create_offer().expect("offer");
    pc2.set_local_description(&offer).expect("pc2 local");
    pc1.set_remote_description(&offer).expect("pc1 remote");

    let tx1 = pc1
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc1 video transceiver");
    tx1.set_direction(TransceiverDirection::SendOnly)
        .expect("send direction");
    let video = sender
        .create_video_track("buffering-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    let answer = pc1.create_answer().expect("answer");
    pc1.set_local_description(&answer).expect("pc1 local");
    pc2.set_remote_description(&answer).expect("pc2 remote");

    let stop = AtomicBool::new(false);
    let mut last = None;
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut seed = 0u8;
            while !stop.load(Ordering::SeqCst) {
                let bgra: Vec<u8> = (0..(W * H * 4) as usize)
                    .map(|i| (i as u8).wrapping_add(seed))
                    .collect();
                video
                    .push_frame(VideoFrame::new(&bgra, W, H))
                    .expect("push frame");
                seed = seed.wrapping_add(7);
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            if s2.connected.load(Ordering::SeqCst) {
                let report = pc2.get_stats().expect("pc2 get_stats");
                let inbound = report
                    .inbound_rtp
                    .into_iter()
                    .find(|s| s.kind == StreamKind::Video);
                if let Some(s) = inbound {
                    let done = s.jitter_buffer_emitted_count >= FRAMES;
                    last = Some(s);
                    if done {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let s = last.expect("no inbound video stats within 30 s");
    assert!(
        s.jitter_buffer_emitted_count >= FRAMES,
        "only {} frames left the jitter buffer within 30 s",
        s.jitter_buffer_emitted_count
    );
    let n = s.jitter_buffer_emitted_count as f64;
    let average = s.average_jitter_buffer_delay().expect("emitted frames");
    let target = Duration::from_secs_f64(s.jitter_buffer_target_delay_s / n);
    (average, target)
}

fn factory() -> PeerConnectionFactory {
    PeerConnectionFactory::builder().build().expect("factory")
}

/// Forcing the receive playout delay holds frames at least that long, without
/// anything from the sender.
#[test]
fn receive_playout_delay_holds_frames() {
    let receiver = PeerConnectionFactory::builder()
        .with_receive_playout_delay(PlayoutDelay::new(
            Duration::from_millis(250),
            Duration::from_millis(250),
        ))
        .build()
        .expect("receiver factory");
    let (average, _) = average_receive_delays(&factory(), &receiver, |_| {});
    println!("receive playout delay 250 ms: average jitter buffer delay {average:?}");
    assert!(
        average >= Duration::from_millis(150),
        "frames waited {average:?} on average, under a forced 250 ms playout delay"
    );
}

/// A playout delay set on the sending factory reaches the receiver through the
/// RTP header extension, and the receiver honours it.
#[test]
fn send_playout_delay_reaches_the_receiver() {
    let sender = PeerConnectionFactory::builder()
        .with_send_playout_delay(PlayoutDelay::new(
            Duration::from_millis(250),
            Duration::from_millis(250),
        ))
        .build()
        .expect("sender factory");
    let (average, _) = average_receive_delays(&sender, &factory(), |_| {});
    println!("send playout delay 250 ms: average jitter buffer delay {average:?}");
    assert!(
        average >= Duration::from_millis(150),
        "frames waited {average:?} on average; the sender asked for 250 ms"
    );
}

/// Immediate playout on receive takes out the render-delay smoothing a default
/// receiver adds, so frames reach the decoder sooner than with the default.
#[test]
fn immediate_receive_playout_beats_the_default() {
    let (default_average, _) = average_receive_delays(&factory(), &factory(), |_| {});
    let immediate = PeerConnectionFactory::builder()
        .with_receive_playout_delay(PlayoutDelay::IMMEDIATE)
        .build()
        .expect("immediate factory");
    let (immediate_average, _) = average_receive_delays(&factory(), &immediate, |_| {});
    println!(
        "average jitter buffer delay: default {default_average:?}, immediate {immediate_average:?}"
    );
    assert!(
        immediate_average < default_average,
        "immediate playout waited {immediate_average:?} on average, the default {default_average:?}"
    );
}

/// A jitter buffer minimum delay set before negotiation holds from the first
/// frame, and the receiver reports it as the target it worked to.
#[test]
fn jitter_buffer_minimum_delay_set_before_the_stream_holds() {
    let (average, target) = average_receive_delays(&factory(), &factory(), |rx| {
        rx.set_jitter_buffer_minimum_delay(Some(Duration::from_millis(300)))
            .expect("set jitter buffer minimum delay");
    });
    println!("jitter buffer minimum 300 ms: average delay {average:?}, target {target:?}");
    assert!(
        target >= Duration::from_millis(250),
        "the receiver reported a {target:?} target delay; a 300 ms minimum was set before the stream"
    );
    assert!(
        average >= Duration::from_millis(200),
        "frames waited {average:?} on average, under a 300 ms jitter buffer minimum"
    );
}

#[test]
fn out_of_range_settings_are_refused() {
    let inverted = PlayoutDelay::new(Duration::from_millis(200), Duration::from_millis(100));
    assert!(PeerConnectionFactory::builder()
        .with_send_playout_delay(inverted)
        .build()
        .is_err());
    let too_long = PlayoutDelay::new(Duration::ZERO, Duration::from_secs(41));
    assert!(PeerConnectionFactory::builder()
        .with_receive_playout_delay(too_long)
        .build()
        .is_err());

    let factory = factory();
    let (pc, _) = make_peer(&factory);
    let tx = pc
        .add_transceiver(MediaKind::Video, TransceiverDirection::RecvOnly)
        .expect("transceiver");
    assert!(tx
        .set_jitter_buffer_minimum_delay(Some(Duration::from_secs(11)))
        .is_err());
    tx.set_jitter_buffer_minimum_delay(None)
        .expect("clearing the minimum delay");
}
