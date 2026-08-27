//! Demonstrates all video-track patterns side by side — all through
//! [`PeerConnectionFactory::create_video_track`]/[`create_video_track_with_options`]
//! with per-track [`VideoTrackOptions`].
//!
//! Three scenarios run in sequence, each creating a fresh factory:
//!
//! 1. **Single pre-encoded track** — one
//!    [`TrackVideoEncoder::PreEncoded`] option.
//!
//! 2. **Multiple pre-encoded tracks** — two of them on one factory;
//!    each track is driven by an independent queue; frames never cross.
//!
//! 3. **Mixed raw + pre-encoded + audio** — a plain raw
//!    [`create_video_track`](PeerConnectionFactory::create_video_track)
//!    (libwebrtc encodes BGRA) next to a pre-encoded track (your bitstream
//!    goes directly to the RTP stack), plus an audio track.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run --example multi_track
//! ```

fn main() {
    #[cfg(not(have_libwebrtc))]
    {
        eprintln!("Set REACTOR_WEBRTC_LIB_DIR=<path/to/dist> to link a native libwebrtc build.");
    }
    #[cfg(have_libwebrtc)]
    run();
}

#[cfg(have_libwebrtc)]
fn run() {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        EncodedVideoFrame, EncodedVideoTrack, FrameAction, FrameTransform, IceCandidate,
        LocalVideoTrack, MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
        PeerConnectionState, PreEncodedOptions, RemoteTrack, RtcConfiguration, TrackVideoEncoder,
        TransceiverDirection, VideoTrackOptions,
    };

    // ── shared peer boilerplate ───────────────────────────────────────────────

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        tracks: Mutex<Vec<RemoteTrack>>,
    }

    fn make_peer(
        factory: &PeerConnectionFactory,
        cfg: &RtcConfiguration,
    ) -> (PeerConnection, Arc<Peer>) {
        let shared = Arc::new(Peer::default());
        let obs = PeerConnectionObserver::new()
            .on_ice_candidate({
                let s = shared.clone();
                move |c| s.ice.lock().unwrap().push_back(c)
            })
            .on_connection_state_change({
                let s = shared.clone();
                move |st| {
                    if st == PeerConnectionState::Connected {
                        s.connected.store(true, Ordering::SeqCst);
                    }
                }
            })
            .on_track({
                let s = shared.clone();
                move |track| s.tracks.lock().unwrap().push(track)
            });
        let pc = factory
            .create_peer_connection(cfg, obs)
            .expect("peer connection");
        (pc, shared)
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

    fn wait_until(
        s1: &Peer,
        s2: &Peer,
        pc1: &PeerConnection,
        pc2: &PeerConnection,
        done: impl Fn() -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            trickle(s1, pc2);
            trickle(s2, pc1);
            if done() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stub_frame(idx: u32) -> EncodedVideoFrame {
        let is_key = idx == 0 || idx % 30 == 0;
        let mut data = vec![if is_key { 0xAA } else { 0xBB }; 64];
        data[1..5].copy_from_slice(&idx.to_be_bytes());
        EncodedVideoFrame {
            data,
            is_key_frame: is_key,
            width: 0,
            height: 0,
            rtp_timestamp: 0,
        }
    }

    // ── the API this example is about: plain factory, per-track options ───────

    fn new_factory() -> PeerConnectionFactory {
        PeerConnectionFactory::builder().build().expect("factory")
    }

    fn pre_encoded_track(
        factory: &PeerConnectionFactory,
        id: &str,
        w: u32,
        h: u32,
    ) -> EncodedVideoTrack {
        let mut options = VideoTrackOptions::default();
        options.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(w, h)));
        match factory
            .create_video_track_with_options(id, options)
            .expect("encoded track")
        {
            LocalVideoTrack::Encoded(t) => t,
            LocalVideoTrack::Raw(_) => panic!("expected a pre-encoded track"),
        }
    }

    let cfg = RtcConfiguration::default();

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 1 — single pre-encoded track (convenience API)
    // ═══════════════════════════════════════════════════════════════════════════
    println!("\n── Scenario 1: single pre-encoded track ──");
    {
        // ┌── factory + one push handle ────────────────────────────────────────
        let factory = new_factory();
        let video = pre_encoded_track(&factory, "cam", 640, 480);
        // └─────────────────────────────────────────────────────────────────────

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        let tx = pc1
            .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
            .expect("tx");
        tx.set_track(video.track()).expect("set track");

        negotiate(&pc1, &pc2);

        let recv = Arc::new(AtomicU32::new(0));
        let tf = FrameTransform::new({
            let recv = recv.clone();
            move |f| {
                if !f.data.is_empty() {
                    recv.fetch_add(1, Ordering::SeqCst);
                }
                FrameAction::Drop
            }
        });
        // The receiving transceiver only materialises once the remote offer
        // has been applied — attach after negotiation.
        pc2.transceivers()
            .into_iter()
            .find(|t| t.kind() == MediaKind::Video)
            .expect("rx transceiver")
            .set_receiver_transform(&tf)
            .expect("transform");

        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0u32.. {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    // ┌── developer-facing push ────────────────────────────────
                    video.push_frame(stub_frame(i));
                    // └─────────────────────────────────────────────────────────
                    thread::sleep(Duration::from_millis(33));
                }
            });
            let ok = wait_until(&s1, &s2, &pc1, &pc2, || recv.load(Ordering::SeqCst) >= 3);
            stop.store(true, Ordering::SeqCst);
            println!(
                "  received {} frame(s) — {}",
                recv.load(Ordering::SeqCst),
                if ok { "✅" } else { "❌ timeout" }
            );
        });
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 2 — two independent pre-encoded tracks
    // ═══════════════════════════════════════════════════════════════════════════
    println!("\n── Scenario 2: two pre-encoded tracks ──");
    {
        // ┌── two pre-encoded tracks on one factory ────────────────────────────
        let factory = new_factory();
        let camera = pre_encoded_track(&factory, "camera", 1280, 720);
        let screen = pre_encoded_track(&factory, "screen", 1920, 1080);
        // └─────────────────────────────────────────────────────────────────────

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        for t in [camera.track(), screen.track()] {
            let tx = pc1
                .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
                .expect("tx");
            tx.set_track(t).expect("set track");
        }

        let recv_cam = Arc::new(AtomicU32::new(0));
        let recv_scr = Arc::new(AtomicU32::new(0));
        negotiate(&pc1, &pc2);

        // Attach a FrameTransform to each receive transceiver.
        for (i, tx) in pc2
            .transceivers()
            .into_iter()
            .filter(|t| t.kind() == MediaKind::Video)
            .enumerate()
        {
            let counter = if i == 0 {
                recv_cam.clone()
            } else {
                recv_scr.clone()
            };
            let tf = FrameTransform::new(move |f| {
                if !f.data.is_empty() {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                FrameAction::Drop
            });
            tx.set_receiver_transform(&tf).expect("transform");
        }

        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0u32.. {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    // ┌── each track pushed independently ──────────────────────
                    camera.push_frame(stub_frame(i)).expect("push frame");
                    screen.push_frame(stub_frame(i)).expect("push frame");
                    // └─────────────────────────────────────────────────────────
                    thread::sleep(Duration::from_millis(33));
                }
            });
            let ok = wait_until(&s1, &s2, &pc1, &pc2, || {
                recv_cam.load(Ordering::SeqCst) >= 3 && recv_scr.load(Ordering::SeqCst) >= 3
            });
            stop.store(true, Ordering::SeqCst);
            println!(
                "  camera: {} frame(s)  screen: {} frame(s) — {}",
                recv_cam.load(Ordering::SeqCst),
                recv_scr.load(Ordering::SeqCst),
                if ok { "✅" } else { "❌ timeout" }
            );
        });
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 3 — raw track + pre-encoded track + audio, same factory
    // ═══════════════════════════════════════════════════════════════════════════
    println!("\n── Scenario 3: raw video + pre-encoded video + audio ──");
    {
        // ┌── raw video + pre-encoded video + audio, one factory ───────────────
        let factory = new_factory();
        let camera = factory
            .create_video_track("camera") // BGRA → libwebrtc encodes
            .expect("video track");
        let screen = pre_encoded_track(&factory, "screen", 1920, 1080); // your bitstream → RTP

        // Audio track from the same factory (synthetic ADM — push PCM manually)
        let audio = factory.create_audio_track("mic").expect("audio track");
        // └─────────────────────────────────────────────────────────────────────

        let (pc1, s1) = make_peer(&factory, &cfg);
        let (pc2, s2) = make_peer(&factory, &cfg);

        // Add video transceivers
        for t in [&camera, screen.track()] {
            let tx = pc1
                .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
                .expect("tx");
            tx.set_track(t).expect("set track");
        }
        // Add audio transceiver
        let atx = pc1
            .add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly)
            .expect("audio tx");
        atx.set_track(&audio).expect("set audio track");

        let recv_vid = Arc::new(AtomicU32::new(0));
        let recv_aud = Arc::new(AtomicU32::new(0));
        negotiate(&pc1, &pc2);

        // Count received video frames via FrameTransform
        for tx in pc2
            .transceivers()
            .into_iter()
            .filter(|t| t.kind() == MediaKind::Video)
        {
            let c = recv_vid.clone();
            let tf = FrameTransform::new(move |f| {
                if !f.data.is_empty() {
                    c.fetch_add(1, Ordering::SeqCst);
                }
                FrameAction::Drop
            });
            tx.set_receiver_transform(&tf).expect("transform");
        }

        let stop = AtomicBool::new(false);
        let (w, h) = (1280u32, 720u32);
        let bgra = vec![128u8; (w * h * 4) as usize];

        thread::scope(|scope| {
            scope.spawn(|| {
                let rate = 48_000u32;
                let block = (rate / 100) as usize; // 10ms @ 48kHz
                let mut i = 0u32;
                let mut si = 0usize;
                while !stop.load(Ordering::SeqCst) {
                    // ┌── raw BGRA → libwebrtc encodes ──────────────────────────
                    camera
                        .push_frame(reactor_webrtc::VideoFrame::new(&bgra, w, h))
                        .expect("push frame");
                    // └──────────────────────────────────────────────────────────

                    // ┌── pre-encoded → straight to RTP ──────────────────────────
                    screen.push_frame(stub_frame(i)).expect("push frame");
                    // └───────────────────────────────────────────────────────────

                    // ┌── synthetic PCM audio ────────────────────────────────────
                    let pcm: Vec<i16> = (si..si + block * 3)
                        .map(|n| (((n % 109) as i32 - 54) * 500) as i16)
                        .collect();
                    factory.push_audio_frame(&pcm, rate, 1);
                    si = si.wrapping_add(block * 3);
                    // └───────────────────────────────────────────────────────────

                    i += 1;
                    thread::sleep(Duration::from_millis(33));
                }
            });

            let ok = wait_until(&s1, &s2, &pc1, &pc2, || {
                recv_vid.load(Ordering::SeqCst) >= 4
            });
            stop.store(true, Ordering::SeqCst);
            println!(
                "  video frames received: {} — {}",
                recv_vid.load(Ordering::SeqCst),
                if ok { "✅" } else { "❌ timeout" }
            );
        });
    }

    println!();
}
