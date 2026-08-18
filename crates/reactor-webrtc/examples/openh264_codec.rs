//! Encode and decode H.264 via a runtime-downloaded OpenH264 (Cisco's
//! prebuilt binary, dynamically loaded — never compiled in; see the
//! `openh264` module's docs for why).
//!
//! Same shape as `builtin_codec.rs`, except the video transceiver is pinned
//! to H264 via `set_codec_preferences` so it actually exercises the OpenH264
//! path instead of falling back to VP8/VP9/AV1.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run --example openh264_codec --features openh264
//! ```
//!
//! First run downloads Cisco's OpenH264 library for your platform (a few
//! hundred KB) and caches it — see `openh264::ensure_available`'s docs for
//! where. Requires Linux or Windows; on macOS/iOS/Android use the platform's
//! hardware H.264 instead (no OpenH264 needed there).

fn main() {
    #[cfg(not(all(have_libwebrtc, feature = "openh264")))]
    {
        eprintln!(
            "This example needs both a linked libwebrtc (REACTOR_WEBRTC_LIB_DIR) and \
             `--features openh264`. See the README for download instructions."
        );
    }
    #[cfg(all(have_libwebrtc, feature = "openh264"))]
    run();
}

// ── everything below requires a linked libwebrtc + the openh264 feature ─────

#[cfg(all(have_libwebrtc, feature = "openh264"))]
fn run() {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        openh264, AdmMode, ApmConfig, IceCandidate, MediaKind, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
        Track, TransceiverDirection, VideoCodec,
    };

    // Required by Cisco's binary license: show this in your app's
    // licensing/EULA surface wherever else you present such notices.
    println!("{}", openh264::OPENH264_ATTRIBUTION);

    println!("Ensuring OpenH264 is available (downloads on first run)…");
    let lib_path = openh264::ensure_available(None).expect(
        "OpenH264 download/verify failed — see openh264::OpenH264Error for what went wrong",
    );
    println!("Using OpenH264 at {}", lib_path.display());

    // ── shared state per peer (identical to builtin_codec.rs) ────────────────

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        video_frames: AtomicU32,
        audio_frames: AtomicU32,
        tracks: Mutex<Vec<Track>>,
    }

    fn make_peer(
        factory: &PeerConnectionFactory,
        config: &RtcConfiguration,
    ) -> (PeerConnection, Arc<Peer>) {
        let shared = Arc::new(Peer::default());
        let obs = PeerConnectionObserver::new()
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
                move |kind, track| {
                    match kind {
                        MediaKind::Video => {
                            let s = s.clone();
                            track.on_video_frame(move |f| {
                                println!(
                                    "  [pc2 video/H264] {}×{} frame — {} bytes BGRA",
                                    f.width,
                                    f.height,
                                    f.bgra.len()
                                );
                                s.video_frames.fetch_add(1, Ordering::SeqCst);
                            });
                        }
                        MediaKind::Audio => {
                            let s = s.clone();
                            track.on_audio_frame(move |_f| {
                                s.audio_frames.fetch_add(1, Ordering::SeqCst);
                            });
                        }
                        MediaKind::Unknown => {}
                    }
                    s.tracks.lock().unwrap().push(track);
                }
            });
        let pc = factory
            .create_peer_connection(config, obs)
            .expect("create peer connection");
        (pc, shared)
    }

    fn trickle(from: &Peer, to: &PeerConnection) {
        while let Some(c) = from.ice.lock().unwrap().pop_front() {
            let _ = to.add_ice_candidate(&c);
        }
    }

    // ── factory (real OpenH264 encode/decode) and peers ──────────────────────

    let factory =
        PeerConnectionFactory::with_openh264(&lib_path, AdmMode::Synthetic, ApmConfig::default())
            .expect("factory with openh264");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // ── local tracks, pinned to H264 on pc1's send side ──────────────────────

    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");
    let audio = factory
        .create_audio_track("reactor-audio")
        .expect("audio track");

    let video_tx = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("add video transceiver");
    video_tx.set_track(&video).expect("set video track");
    video_tx
        .set_codec_preferences(&[VideoCodec::H264])
        .expect("prefer H264");
    pc1.add_track(&audio).expect("add audio track");

    // ── SDP offer / answer ────────────────────────────────────────────────────

    let offer = pc1.create_offer().expect("create offer");
    println!("Offer m= lines:");
    for line in offer.sdp.lines().filter(|l| l.starts_with("m=")) {
        println!("  {line}");
    }
    assert!(
        offer.sdp.to_lowercase().contains("h264"),
        "offer should list H264 first for the video m-line"
    );

    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");

    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    // ── media pump + ICE trickle loop (video only needs to prove it decodes) ─

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let mut bgra = vec![0u8; (w * h * 4) as usize];
            let mut phase = 0u8;
            let (rate, channels) = (48_000u32, 1u32);
            let samples_per_10ms = (rate / 100) as usize;
            let mut sample_idx = 0usize;

            while !stop.load(Ordering::SeqCst) {
                for (i, b) in bgra.iter_mut().enumerate() {
                    *b = ((i as u32 + phase as u32 * 7) % 256) as u8;
                }
                video.push_video_frame(&bgra, w, h);
                phase = phase.wrapping_add(1);

                for _ in 0..3 {
                    let pcm: Vec<i16> = (sample_idx..sample_idx + samples_per_10ms)
                        .map(|i| (((i % 109) as i32 - 54) * 500) as i16)
                        .collect();
                    factory.push_audio_frame(&pcm, rate, channels);
                    sample_idx = sample_idx.wrapping_add(samples_per_10ms);
                    thread::sleep(Duration::from_millis(10));
                }
            }
        });

        println!("Waiting for ICE connection and decoded H264 video…");
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);

            let both_up =
                s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);
            let got_video = s2.video_frames.load(Ordering::SeqCst) >= 3;

            if (both_up && got_video) || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let v = s2.video_frames.load(Ordering::SeqCst);
    let connected = s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);

    if connected && v > 0 {
        println!("\nopenh264_codec ✅ — pc2 decoded {v} H264 video frame(s)");
    } else {
        eprintln!("\nopenh264_codec ❌ — connected={connected} video={v}");
        std::process::exit(1);
    }
}
