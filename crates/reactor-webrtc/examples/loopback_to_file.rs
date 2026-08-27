//! loopback-to-file — a first-class `reactor-webrtc` example.
//!
//! Two PeerConnections in one process: the sender generates a moving video
//! pattern + a sine tone and streams them; the receiver writes whatever it
//! decodes to disk — video as YUV4MPEG2 (`.y4m`) and audio as WAV (`.wav`).
//! Uses only the safe API (closures + RAII).
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run -p reactor-webrtc --example loopback_to_file -- ./out 3
//! # → ./out/loopback.y4m  ./out/loopback.wav
//! ```

#[cfg(not(have_libwebrtc))]
fn main() {
    eprintln!(
        "this example needs a native libwebrtc — set REACTOR_WEBRTC_LIB_DIR \
         (a webrtc-build/out/<target>/dist dir) or REACTOR_WEBRTC_PREBUILT_URL \
         and rebuild."
    );
}

#[cfg(have_libwebrtc)]
fn main() {
    imp::run();
}

#[cfg(have_libwebrtc)]
mod imp {
    use std::collections::VecDeque;
    use std::fs::{self, File};
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use reactor_webrtc::{
        MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
        PeerConnectionState, RtcConfiguration, Track,
    };

    const W: usize = 320;
    const H: usize = 240;
    const RATE: u32 = 48_000;
    const CHANNELS: u32 = 2;

    pub fn run() {
        let mut args = std::env::args().skip(1);
        let out_dir = args.next().unwrap_or_else(|| "loopback-out".into());
        let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
        fs::create_dir_all(&out_dir).expect("create out dir");
        let y4m_path = Path::new(&out_dir).join("loopback.y4m");
        let wav_path = Path::new(&out_dir).join("loopback.wav");

        let video_writer = Arc::new(Mutex::new(Y4mWriter::new(&y4m_path)));
        let wav_writer = Arc::new(Mutex::new(WavWriter::new(&wav_path)));
        let frames = Arc::new(AtomicU32::new(0));
        let blocks = Arc::new(AtomicU32::new(0));

        let factory = PeerConnectionFactory::builder().build().expect("factory");
        let config = RtcConfiguration::default();

        // Receiver: write decoded media to the files.
        let recv_tracks: Arc<Mutex<Vec<Track>>> = Arc::new(Mutex::new(Vec::new()));
        let (recv_pc, recv_state) = make_peer(&factory, &config, {
            let vw = video_writer.clone();
            let aw = wav_writer.clone();
            let frames = frames.clone();
            let blocks = blocks.clone();
            let recv_tracks = recv_tracks.clone();
            move |kind, mut track| {
                match kind {
                    MediaKind::Video => {
                        let vw = vw.clone();
                        let frames = frames.clone();
                        track.on_video_frame(move |f| {
                            vw.lock()
                                .unwrap()
                                .write(f.bgra, f.width as usize, f.height as usize);
                            frames.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                    MediaKind::Audio => {
                        let aw = aw.clone();
                        let blocks = blocks.clone();
                        track.on_audio_frame(move |f| {
                            aw.lock().unwrap().write(f.pcm, f.sample_rate, f.channels);
                            blocks.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                    MediaKind::Unknown => {}
                }
                recv_tracks.lock().unwrap().push(track);
            }
        });

        // Sender: a video + an audio track.
        let (send_pc, send_state) = make_peer(&factory, &config, |_, _| {});
        let video = factory.create_video_track("video").expect("video track");
        let audio = factory.create_audio_track("audio").expect("audio track");
        send_pc.add_track(&video).expect("add video");
        send_pc.add_track(&audio).expect("add audio");

        // Offer/answer (sender → receiver).
        let offer = send_pc.create_offer().expect("offer");
        send_pc
            .set_local_description(&offer)
            .expect("send local offer");
        recv_pc
            .set_remote_description(&offer)
            .expect("recv remote offer");
        let answer = recv_pc.create_answer().expect("answer");
        recv_pc
            .set_local_description(&answer)
            .expect("recv local answer");
        send_pc
            .set_remote_description(&answer)
            .expect("send remote answer");
        println!("negotiated; capturing ~{seconds}s to {out_dir}/ …");

        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            // Trickle ICE continuously, both ways.
            scope.spawn(|| {
                while !stop.load(Ordering::SeqCst) {
                    forward_ice(&send_state, &recv_pc);
                    forward_ice(&recv_state, &send_pc);
                    thread::sleep(Duration::from_millis(20));
                }
            });
            // Generate + push media.
            scope.spawn(|| {
                let spc = (RATE / 100) as usize; // 10ms
                let mut bgra = vec![0u8; W * H * 4];
                let mut pcm = vec![0i16; spc * CHANNELS as usize];
                let mut phase = 0.0f32;
                let mut tick = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    fill_tone(&mut pcm, &mut phase, CHANNELS as usize);
                    factory.push_audio_frame(&pcm, RATE, CHANNELS);
                    if tick % 3 == 0 {
                        fill_pattern(&mut bgra, tick);
                        video.push_video_frame(&bgra, W as u32, H as u32);
                    }
                    tick = tick.wrapping_add(1);
                    thread::sleep(Duration::from_millis(10));
                }
            });

            // Wait for connection, then capture for `seconds`.
            let start = Instant::now();
            while !(send_state.connected.load(Ordering::SeqCst)
                && recv_state.connected.load(Ordering::SeqCst))
                && start.elapsed() < Duration::from_secs(10)
            {
                thread::sleep(Duration::from_millis(50));
            }
            thread::sleep(Duration::from_secs(seconds));
            stop.store(true, Ordering::SeqCst);
        });

        wav_writer.lock().unwrap().finalize();
        println!(
            "done — wrote {} video frames → {} and {} audio blocks → {}",
            frames.load(Ordering::SeqCst),
            y4m_path.display(),
            blocks.load(Ordering::SeqCst),
            wav_path.display(),
        );
    }

    // ── observer helpers ─────────────────────────────────────────────────────

    struct State {
        ice: Mutex<VecDeque<reactor_webrtc::IceCandidate>>,
        connected: AtomicBool,
    }

    fn make_peer(
        factory: &PeerConnectionFactory,
        config: &RtcConfiguration,
        on_track: impl FnMut(MediaKind, Track) + Send + 'static,
    ) -> (PeerConnection, Arc<State>) {
        let state = Arc::new(State {
            ice: Mutex::new(VecDeque::new()),
            connected: AtomicBool::new(false),
        });
        let observer = PeerConnectionObserver::new()
            .on_ice_candidate({
                let s = state.clone();
                move |c| s.ice.lock().unwrap().push_back(c)
            })
            .on_connection_state_change({
                let s = state.clone();
                move |st| {
                    if st == PeerConnectionState::Connected {
                        s.connected.store(true, Ordering::SeqCst);
                    }
                }
            })
            .on_track(on_track);
        let pc = factory
            .create_peer_connection(config, observer)
            .expect("create peer connection");
        (pc, state)
    }

    fn forward_ice(from: &State, to: &PeerConnection) {
        while let Some(c) = {
            let mut q = from.ice.lock().unwrap();
            q.pop_front()
        } {
            let _ = to.add_ice_candidate(&c);
        }
    }

    // ── content generation ─────────────────────────────────────────────────────

    /// A scrolling BGRA gradient.
    fn fill_pattern(bgra: &mut [u8], tick: u32) {
        for y in 0..H {
            for x in 0..W {
                let p = (y * W + x) * 4;
                bgra[p] = (x + tick as usize) as u8; // B
                bgra[p + 1] = (y + tick as usize) as u8; // G
                bgra[p + 2] = (x + y) as u8; // R
                bgra[p + 3] = 0xff; // A
            }
        }
    }

    /// A 440Hz sine, interleaved across channels.
    fn fill_tone(pcm: &mut [i16], phase: &mut f32, channels: usize) {
        let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
        for frame in pcm.chunks_mut(channels) {
            let s = (phase.sin() * 8000.0) as i16;
            for c in frame.iter_mut() {
                *c = s;
            }
            *phase += step;
            if *phase > 2.0 * std::f32::consts::PI {
                *phase -= 2.0 * std::f32::consts::PI;
            }
        }
    }

    // ── file writers ─────────────────────────────────────────────────────────

    /// Streams I420 frames as YUV4MPEG2. Converts incoming BGRA → I420 (BT.601).
    struct Y4mWriter {
        file: File,
        header: bool,
    }

    impl Y4mWriter {
        fn new(path: &Path) -> Self {
            Self {
                file: File::create(path).expect("create y4m"),
                header: false,
            }
        }

        fn write(&mut self, bgra: &[u8], w: usize, h: usize) {
            if w == 0 || h == 0 || bgra.len() < w * h * 4 {
                return;
            }
            if !self.header {
                let _ = writeln!(self.file, "YUV4MPEG2 W{w} H{h} F30:1 Ip A1:1 C420");
                self.header = true;
            }
            let (cw, ch) = (w / 2, h / 2);
            let mut plane = Vec::with_capacity(w * h + 2 * cw * ch);
            // Y
            for j in 0..h {
                for i in 0..w {
                    let p = (j * w + i) * 4;
                    let (b, g, r) = (bgra[p] as f32, bgra[p + 1] as f32, bgra[p + 2] as f32);
                    plane.push(clamp8(0.257 * r + 0.504 * g + 0.098 * b + 16.0));
                }
            }
            // U then V (4:2:0, top-left sampled)
            for &(cu, cv) in &[(true, false), (false, true)] {
                for j in (0..h).step_by(2) {
                    for i in (0..w).step_by(2) {
                        let p = (j * w + i) * 4;
                        let (b, g, r) = (bgra[p] as f32, bgra[p + 1] as f32, bgra[p + 2] as f32);
                        let val = if cu {
                            -0.148 * r - 0.291 * g + 0.439 * b + 128.0
                        } else if cv {
                            0.439 * r - 0.368 * g - 0.071 * b + 128.0
                        } else {
                            128.0
                        };
                        plane.push(clamp8(val));
                    }
                }
            }
            let _ = self.file.write_all(b"FRAME\n");
            let _ = self.file.write_all(&plane);
        }
    }

    fn clamp8(v: f32) -> u8 {
        v.clamp(0.0, 255.0) as u8
    }

    /// Writes interleaved s16le PCM as a WAV file (header patched on finalize).
    struct WavWriter {
        file: File,
        sample_rate: u32,
        channels: u32,
        data_bytes: u32,
        header: bool,
    }

    impl WavWriter {
        fn new(path: &Path) -> Self {
            Self {
                file: File::create(path).expect("create wav"),
                sample_rate: RATE,
                channels: CHANNELS,
                data_bytes: 0,
                header: false,
            }
        }

        fn write(&mut self, pcm: &[i16], sample_rate: u32, channels: u32) {
            if !self.header {
                self.sample_rate = sample_rate;
                self.channels = channels.max(1);
                self.write_header(0);
                self.header = true;
            }
            let mut bytes = Vec::with_capacity(pcm.len() * 2);
            for s in pcm {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            if self.file.write_all(&bytes).is_ok() {
                self.data_bytes += bytes.len() as u32;
            }
        }

        fn write_header(&mut self, data_bytes: u32) {
            let byte_rate = self.sample_rate * self.channels * 2;
            let block_align = (self.channels * 2) as u16;
            let _ = self.file.seek(SeekFrom::Start(0));
            let _ = self.file.write_all(b"RIFF");
            let _ = self.file.write_all(&(36 + data_bytes).to_le_bytes());
            let _ = self.file.write_all(b"WAVEfmt ");
            let _ = self.file.write_all(&16u32.to_le_bytes());
            let _ = self.file.write_all(&1u16.to_le_bytes()); // PCM
            let _ = self.file.write_all(&(self.channels as u16).to_le_bytes());
            let _ = self.file.write_all(&self.sample_rate.to_le_bytes());
            let _ = self.file.write_all(&byte_rate.to_le_bytes());
            let _ = self.file.write_all(&block_align.to_le_bytes());
            let _ = self.file.write_all(&16u16.to_le_bytes()); // bits
            let _ = self.file.write_all(b"data");
            let _ = self.file.write_all(&data_bytes.to_le_bytes());
        }

        fn finalize(&mut self) {
            if self.header {
                let n = self.data_bytes;
                self.write_header(n);
                let _ = self.file.seek(SeekFrom::End(0));
            }
        }
    }
}
