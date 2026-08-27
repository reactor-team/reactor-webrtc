//! Encode and decode video with libwebrtc's built-in codecs (VP8 / VP9 / AV1).
//!
//! Two `PeerConnection`s negotiate over local ICE. pc1 pushes raw BGRA video
//! and interleaved i16 PCM audio; libwebrtc encodes both, transmits them, and
//! decodes them on arrival. pc2 receives BGRA frames in `on_video_frame` and
//! PCM in `on_audio_frame`.
//!
//! This is the zero-integration path: no custom encoder, no codec knowledge
//! required. Swap `PeerConnectionFactory::builder().build()` for
//! `PeerConnectionFactory::builder().with_platform_adm().build()` to capture
//! from a real microphone instead of synthetic audio.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run --example builtin_codec
//! ```

fn main() {
    #[cfg(not(have_libwebrtc))]
    {
        eprintln!(
            "Set REACTOR_WEBRTC_LIB_DIR=<path/to/dist> to link a native libwebrtc build.\n\
             See the README for download instructions."
        );
    }
    #[cfg(have_libwebrtc)]
    run();
}

// ── everything below requires a linked libwebrtc ─────────────────────────────

#[cfg(have_libwebrtc)]
fn run() {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        IceCandidate, MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
        PeerConnectionState, RtcConfiguration, Track,
    };

    // ── shared state per peer ─────────────────────────────────────────────────

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        video_frames: AtomicU32,
        audio_frames: AtomicU32,
        // Keep received track handles alive so their sinks don't drop.
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
                move |kind, mut track| {
                    match kind {
                        MediaKind::Video => {
                            // Callback fires on the libwebrtc decode thread with
                            // each decoded BGRA frame.
                            let s = s.clone();
                            track.on_video_frame(move |f| {
                                // f.bgra is a Vec<u8>: width × height × 4 bytes.
                                // Feed it to your renderer (Metal, wgpu, SDL2, …).
                                println!(
                                    "  [pc2 video] {}×{} frame — {} bytes BGRA",
                                    f.width,
                                    f.height,
                                    f.bgra.len()
                                );
                                s.video_frames.fetch_add(1, Ordering::SeqCst);
                            });
                        }
                        MediaKind::Audio => {
                            // Callback fires with interleaved i16 PCM samples.
                            let s = s.clone();
                            track.on_audio_frame(move |f| {
                                // f.pcm: interleaved i16, f.channels channels,
                                // f.sample_rate Hz, len = samples_per_channel × channels.
                                println!(
                                    "  [pc2 audio] {}Hz {}ch — {} samples",
                                    f.sample_rate,
                                    f.channels,
                                    f.pcm.len() / f.channels as usize,
                                );
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

    // ── factory and peers ─────────────────────────────────────────────────────

    // Synthetic ADM: no real audio hardware. Push PCM manually below.
    // Switch to PeerConnectionFactory::builder().with_platform_adm().build()
    // to capture from the real microphone and play decoded audio through the
    // speaker.
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // ── local tracks ─────────────────────────────────────────────────────────

    // Video track backed by a push-able source (no camera capture).
    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");

    // Audio track sourced from the synthetic ADM.
    let audio = factory
        .create_audio_track("reactor-audio")
        .expect("audio track");

    // Attach both tracks to pc1's send path. libwebrtc will negotiate and
    // pick the best codec (VP8 / VP9 / AV1 for video, Opus for audio).
    pc1.add_track(&video).expect("add video track");
    pc1.add_track(&audio).expect("add audio track");

    // ── SDP offer / answer ───────────────────────────────────────────────────

    let offer = pc1.create_offer().expect("create offer");
    println!("Offer m= lines:");
    for line in offer.sdp.lines().filter(|l| l.starts_with("m=")) {
        println!("  {line}");
    }

    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");

    let answer = pc2.create_answer().expect("create answer");
    println!("Answer m= lines:");
    for line in answer.sdp.lines().filter(|l| l.starts_with("m=")) {
        println!("  {line}");
    }

    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    // ── media pump + ICE trickle loop ─────────────────────────────────────────

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            // Varying pixel pattern so the encoder emits distinct frames.
            let mut bgra = vec![0u8; (w * h * 4) as usize];
            let mut phase = 0u8;

            // Synthetic mono PCM: a 440 Hz sine-like approximation.
            let (rate, channels) = (48_000u32, 1u32);
            let samples_per_10ms = (rate / 100) as usize;
            let mut sample_idx = 0usize;

            while !stop.load(Ordering::SeqCst) {
                // --- video: push one BGRA frame every ~33ms ---
                for (i, b) in bgra.iter_mut().enumerate() {
                    *b = ((i as u32 + phase as u32 * 7) % 256) as u8;
                }
                video.push_video_frame(&bgra, w, h);
                phase = phase.wrapping_add(1);

                // --- audio: push 10ms of PCM (×3 to match ~30ms video cadence) ---
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

        println!("Waiting for ICE connection and received media…");
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);

            let both_up =
                s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);
            let got_media = s2.video_frames.load(Ordering::SeqCst) >= 3
                && s2.audio_frames.load(Ordering::SeqCst) >= 3;

            if (both_up && got_media) || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    // ── results ───────────────────────────────────────────────────────────────

    let v = s2.video_frames.load(Ordering::SeqCst);
    let a = s2.audio_frames.load(Ordering::SeqCst);
    let connected = s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);

    if connected && v > 0 && a > 0 {
        println!("\nbuiltin_codec ✅ — pc2 received {v} video + {a} audio frame(s)");
    } else {
        eprintln!("\nbuiltin_codec ❌ — connected={connected} video={v} audio={a}");
        std::process::exit(1);
    }
}
