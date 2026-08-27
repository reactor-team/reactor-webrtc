//! Mic + music on one factory, with the **real** microphone.
//!
//! Same scenario as `music_mic_simulated.rs`, but the "mic" track is the
//! platform audio module (CoreAudio / ALSA / WASAPI) with the echo
//! cancellation + noise suppression constraints switched ON for the mic and
//! OFF for the music — per-track, via [`AudioTrackOptions`].
//!
//! ⚠ Requires an actual audio device. On a headless host the platform ADM
//! fails an uncatchable `RTC_CHECK` inside libwebrtc (SIGABRT) — run
//! `music_mic_simulated.rs` there instead.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run --example music_and_mic
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
        AudioTrackOptions, AudioTrackSource, IceCandidate, MediaKind, PeerConnection,
        PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState, RtcConfiguration,
        Track, TransceiverDirection,
    };

    eprintln!(
        "\
        HEADS UP: this example opens the real microphone. It aborts (SIGABRT) \
        on hosts without an audio device — use music_mic_simulated there.\n"
    );

    #[derive(Default)]
    struct Peer {
        ice: Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        mic_frames: AtomicU32,
        music_frames: AtomicU32,
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
                                0 => {
                                    println!(
                                        "  [pc2 mic]   {} samples @ {} Hz",
                                        f.pcm.len(),
                                        f.sample_rate
                                    );
                                    s.mic_frames.fetch_add(1, Ordering::SeqCst);
                                }
                                1 => {
                                    println!(
                                        "  [pc2 music] {} samples @ {} Hz",
                                        f.pcm.len(),
                                        f.sample_rate
                                    );
                                    s.music_frames.fetch_add(1, Ordering::SeqCst);
                                }
                                _ => {}
                            };
                        });
                    }
                    s.recv.lock().unwrap().push(track);
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

    println!("\n── REAL mic (platform ADM, AEC/NS on) + music (LocalPush, all off) ──\n");

    // Platform ADM + full DSP chain on the factory; each track then opts in
    // or out piece by piece.
    let factory = PeerConnectionFactory::builder()
        .with_platform_adm()
        .build()
        .expect("factory with the platform audio device");

    // Mic: real hardware, AEC + NS on for this track's source.
    let mic = factory
        .create_audio_track_with_options("mic", {
            let mut options = AudioTrackOptions::default();
            options.echo_cancellation = Some(true);
            options.noise_suppression = Some(true);
            options
        })
        .expect("mic track");

    // Music: independent push source, processing flat-out OFF — music must
    // never be "echo-cancelled".
    let music = factory
        .create_audio_track_with_options("music", {
            let mut options = AudioTrackOptions::default();
            options.source = AudioTrackSource::LocalPush;
            options.echo_cancellation = Some(false);
            options.noise_suppression = Some(false);
            options.auto_gain_control = Some(false);
            options
        })
        .expect("music track");

    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);
    pc1.add_track(&mic).expect("add mic");
    pc1.add_track(&music).expect("add music");
    negotiate(&pc1, &pc2);

    let stop = AtomicBool::new(false);
    let (rate, channels) = (48_000u32, 2u32);
    let block = (rate / 100) as usize * channels as usize;

    thread::scope(|scope| {
        let stop_ref = &stop;
        scope.spawn(move || {
            let mut t = 0f32;
            while !stop_ref.load(Ordering::SeqCst) {
                // Synthetic music push; the mic track feeds itself from the
                // OS audio device — nothing to call for it.
                let music_pcm: Vec<i16> = (0..block)
                    .map(|i| ((t + i as f32 / rate as f32 * 440.0).sin() * 6_000.0) as i16)
                    .collect();
                music
                    .push_pcm(&music_pcm, rate, channels)
                    .expect("push_pcm");
                t += block as f32 / rate as f32;
                thread::sleep(Duration::from_millis(10));
            }
        });

        println!("Say something — the remote side counts frames from both sources.\n");
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s2.music_frames.load(Ordering::SeqCst) >= 2;
            if done && start.elapsed() > Duration::from_secs(5) {
                break;
            }
            if start.elapsed() > Duration::from_secs(30) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let (m, mu) = (
        s2.mic_frames.load(Ordering::SeqCst),
        s2.music_frames.load(Ordering::SeqCst),
    );
    println!("\nmusic_and_mic — pc2 decoded {m} mic + {mu} music frame(s)");
    if mu == 0 {
        eprintln!("music_and_mic ❌ — no music frames");
        std::process::exit(1);
    }
    if m == 0 {
        eprintln!(
            "note: 0 mic frames — silent room or mic permission denied; \
             the music path alone proves the demo. Run with audio granted to see both."
        );
    }
    println!("music_and_mic ✅");
}
