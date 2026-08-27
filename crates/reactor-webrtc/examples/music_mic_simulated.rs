//! Mic + music on one factory — the `AudioTrackSource` showcase, portable
//! (runs headless).
//!
//! A real mic would need `with_platform_adm()`; this example runs on any host
//! by using the **synthetic** ADM as a stand-in for the microphone: the "mic"
//! track is fed by `factory.push_audio_frame` (the shared ADM pipe) and the
//! "music" track is an independent `LocalPush` source, fed by
//! `track.push_pcm`. The semantics on show are identical — for the real mic,
//! one builder line is all it takes:
//!
//! ```rust,ignore
//! let factory = PeerConnectionFactory::builder()
//!     .with_platform_adm()   // <- the one line that swaps in the real mic
//!     .build()?;
//! ```
//!
//! (See also `music_and_mic.rs`, which uses the real hardware.)
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run --example music_mic_simulated
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

    println!("\n── mic (shared ADM pipe) + music (LocalPush), one factory ──\n");

    // …replace builder().build() with the one-liner in the doc header to use
    // real hardware instead of the stand-in pipe.
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();

    // "mic": sourced from the factory ADM — shared pipe, fed via
    // factory.push_audio_frame. This is where `.with_platform_adm()` would
    // turn the pipe into your actual microphone.
    let mic = factory.create_audio_track("mic").expect("mic track");

    // "music": an independent per-track push source. The point of the demo —
    // an ADM-sourced track and a LocalPush track coexist with independent
    // audio. Processing flags are shown off for completeness: a music track
    // never wants the AEC chain anywhere near it.
    let music = factory
        .create_audio_track_with_options("music", {
            let mut options = AudioTrackOptions::default();
            options.source = AudioTrackSource::LocalPush;
            options.echo_cancellation = Some(false);
            options.noise_suppression = Some(false);
            options
        })
        .expect("music track");

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);
    pc1.add_track(&mic).expect("add mic");
    pc1.add_track(&music).expect("add music");
    negotiate(&pc1, &pc2);

    let stop = AtomicBool::new(false);
    let (rate, channels) = (48_000u32, 2u32);
    let block = (rate / 100) as usize * channels as usize;

    let music_pump = music;
    let factory_pump = factory;
    thread::scope(|scope| {
        let stop_ref = &stop;
        scope.spawn(move || {
            let mut t = 0f32;
            while !stop_ref.load(Ordering::SeqCst) {
                // mic: a 220 Hz sine-ish wave pushed through the ADM pipe.
                let mic_pcm: Vec<i16> = (0..block)
                    .map(|i| ((t + i as f32 / rate as f32 * 220.0).sin() * 8_000.0) as i16)
                    .collect();
                factory_pump.push_audio_frame(&mic_pcm, rate, channels);
                t += block as f32 / rate as f32;

                // music: a 440 Hz counter-melody through the LocalPush track.
                let music_pcm: Vec<i16> = (0..block)
                    .map(|i| ((t + i as f32 / rate as f32 * 440.0).sin() * 6_000.0) as i16)
                    .collect();
                music_pump
                    .push_pcm(&music_pcm, rate, channels)
                    .expect("push_pcm");

                thread::sleep(Duration::from_millis(10));
            }
        });

        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = s2.mic_frames.load(Ordering::SeqCst) >= 2
                && s2.music_frames.load(Ordering::SeqCst) >= 2;
            if done || start.elapsed() > Duration::from_secs(20) {
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
    if m > 0 && mu > 0 {
        println!("\nmusic_mic_simulated ✅ — pc2 decoded {m} mic + {mu} music audio frame(s)");
    } else {
        eprintln!("\nmusic_mic_simulated ❌ — mic={m} music={mu}");
        std::process::exit(1);
    }
}
