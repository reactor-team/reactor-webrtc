//! RGB24 video over a real loopback: a frame pushed as RGB24 arrives as RGB24
//! with its channels in order, a BGRA frame arrives as RGB24 and an RGB24
//! frame as BGRA, and an RGB24 push refuses a buffer of the wrong size.
//!
//! Gated on a native libwebrtc being linked (see build.rs).
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RemoteTrack, RgbFrame, RtcConfiguration, VideoFrame, VideoTrack,
};

const W: u32 = 320;
const H: u32 = 240;

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

/// How a frame is pushed and how it is received.
#[derive(Clone, Copy, Debug)]
enum Format {
    Bgra,
    Rgb,
}

/// Push a solid red frame in `push`, receive it in `receive`, and return the
/// centre pixel the receiver read, as (R, G, B), with the frame's byte length.
fn red_frame_through(push: Format, receive: Format) -> ((u8, u8, u8), usize) {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);
    let video = factory
        .create_video_track("rgb-video")
        .expect("video track");
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

    let received: Arc<Mutex<Option<((u8, u8, u8), usize)>>> = Arc::default();
    let stop = AtomicBool::new(false);
    // The checks report through `outcome` rather than panicking inside the
    // scope: a panic there waits for the pusher, which only `stop` ends.
    let outcome = thread::scope(|scope| {
        scope.spawn(|| {
            let rgb: Vec<u8> = [255u8, 0, 0].repeat((W * H) as usize);
            let bgra: Vec<u8> = [0u8, 0, 255, 255].repeat((W * H) as usize);
            while !stop.load(Ordering::SeqCst) {
                match push {
                    Format::Rgb => video.push_rgb_frame(RgbFrame::new(&rgb, W, H)),
                    Format::Bgra => video.push_frame(VideoFrame::new(&bgra, W, H)),
                }
                .expect("push frame");
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
        let outcome = if connected {
            Ok(s2.video.lock().unwrap().take().expect("remote video"))
        } else {
            Err("loopback did not connect")
        };
        let Ok(remote) = outcome else {
            stop.store(true, Ordering::SeqCst);
            return outcome.map(|_| ());
        };
        let centre = |w: u32, h: u32| ((h / 2) * w + w / 2) as usize;
        match receive {
            Format::Rgb => remote.on_rgb_frame({
                let received = received.clone();
                move |f| {
                    let i = centre(f.width, f.height) * 3;
                    *received.lock().unwrap() =
                        Some(((f.rgb[i], f.rgb[i + 1], f.rgb[i + 2]), f.rgb.len()));
                }
            }),
            Format::Bgra => remote.on_frame({
                let received = received.clone();
                move |f| {
                    let i = centre(f.width, f.height) * 4;
                    *received.lock().unwrap() =
                        Some(((f.bgra[i + 2], f.bgra[i + 1], f.bgra[i]), f.bgra.len()));
                }
            }),
        }
        let got_frame = wait_for(Duration::from_secs(10), || {
            received.lock().unwrap().is_some()
        });
        stop.store(true, Ordering::SeqCst);
        if got_frame {
            Ok(())
        } else {
            Err("no frame received")
        }
    });
    outcome.expect("loopback");
    let got = received.lock().unwrap().take().expect("a frame");
    got
}

fn assert_red((r, g, b): (u8, u8, u8)) {
    // Through an encoder and back, red stays far from the other two channels.
    assert!(
        r > 200 && g < 60 && b < 60,
        "expected red, got ({r}, {g}, {b})"
    );
}

#[test]
fn an_rgb_frame_arrives_as_rgb_with_its_channels_in_order() {
    let (pixel, len) = red_frame_through(Format::Rgb, Format::Rgb);
    assert_red(pixel);
    assert_eq!(len, (W * H * 3) as usize);
}

#[test]
fn a_bgra_frame_arrives_as_rgb() {
    let (pixel, len) = red_frame_through(Format::Bgra, Format::Rgb);
    assert_red(pixel);
    assert_eq!(len, (W * H * 3) as usize);
}

#[test]
fn an_rgb_frame_arrives_as_bgra() {
    let (pixel, len) = red_frame_through(Format::Rgb, Format::Bgra);
    assert_red(pixel);
    assert_eq!(len, (W * H * 4) as usize);
}

#[test]
fn an_rgb_push_refuses_a_buffer_of_the_wrong_size() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let video = factory
        .create_video_track("rgb-video")
        .expect("video track");
    let short = vec![0u8; (W * H * 3 - 1) as usize];
    assert!(video.push_rgb_frame(RgbFrame::new(&short, W, H)).is_err());
    assert!(video.push_rgb_frame(RgbFrame::new(&[], 0, H)).is_err());
    let exact = vec![0u8; 2 * 2 * 3];
    video
        .push_rgb_frame(RgbFrame::new(&exact, 2, 2))
        .expect("an exact buffer is accepted");
}
