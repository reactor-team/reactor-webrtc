//! Send and receive pre-encoded video frames.
//!
//! # Sender side
//!
//! [`PeerConnectionFactory::with_encoded_video_track`] returns `(factory, track)`.
//! Call [`EncodedVideoTrack::push_encoded_frame`] whenever your encoder
//! (VideoToolbox, NVENC, GStreamer, libvpx, …) produces a frame — at your own
//! pace, on any thread. No raw pixel pumping required.
//!
//! The negotiated codec is determined by SDP: the factory advertises VP8, VP9,
//! H264, AV1 and H265; whichever the remote peer accepts first is used. Your
//! encoder must produce the matching bitstream. If you need a specific codec,
//! inspect `a=rtpmap` lines in the answer and only push frames for the agreed
//! codec.
//!
//! # Receiver side
//!
//! A [`FrameTransform`] attached to the receiver sits between the RTP
//! depacketizer and the decoder. Use it to:
//! - Record the encoded stream to disk.
//! - Forward to another peer connection (SFU / MCU).
//! - Push into a hardware decoder (VideoToolbox, MediaCodec).
//!
//! Return [`FrameAction::Drop`] to discard (the null decoder in the factory
//! would drop the frame anyway). Return [`FrameAction::Forward`] to let the
//! builtin decoder run (VP8, VP9, AV1 have software decoders in this build).
//!
//! # Running
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo run --example pre_encoded
//! ```

fn main() {
    #[cfg(not(have_libwebrtc))]
    {
        eprintln!(
            "Set REACTOR_WEBRTC_LIB_DIR=<path/to/dist> to link a native libwebrtc build."
        );
        return;
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
        EncodedVideoFrame, FrameAction, FrameTransform, IceCandidate, MediaKind,
        PeerConnection, PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState,
        RtcConfiguration, Track, TransceiverDirection,
    };

    // ── peer boilerplate ──────────────────────────────────────────────────────

    #[derive(Default)]
    struct Peer {
        ice:       Mutex<VecDeque<IceCandidate>>,
        connected: AtomicBool,
        tracks:    Mutex<Vec<Track>>,
    }

    fn make_peer(
        factory: &PeerConnectionFactory,
        config:  &RtcConfiguration,
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
                move |_kind, track| s.tracks.lock().unwrap().push(track)
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

    // ── factory + encoded video track ─────────────────────────────────────────
    //
    // with_encoded_video_track returns both the factory and a push handle.
    // The factory internally:
    //   1. Creates a CustomVideoEncoder whose closure pops from a shared queue.
    //   2. Creates a video track to trigger the encoder thread.
    // Push a frame → it goes on the queue → the encoder thread dequeues it →
    // RTP packetizer → ICE transport.

    let (w, h) = (640u32, 480u32);
    let (factory, video) =
        PeerConnectionFactory::with_encoded_video_track("cam", w, h)
            .expect("factory");

    let config    = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // ── sender: sendonly transceiver ─────────────────────────────────────────

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    // Attach the pre-encoded track's underlying raw handle.
    tx1.set_track(video.track()).expect("set track");

    // ── offer / answer ────────────────────────────────────────────────────────

    let offer = pc1.create_offer().expect("create offer");

    println!("Codecs offered (m=video):");
    for line in offer.sdp.lines().filter(|l| l.starts_with("a=rtpmap")) {
        println!("  {line}");
    }

    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");

    let answer = pc2.create_answer().expect("create answer");

    // Find the negotiated codec from the answer so our encoder produces the
    // right bitstream. In production you'd parse a=rtpmap to get the codec
    // name and configure your hardware encoder accordingly.
    let negotiated_codec = answer
        .sdp
        .lines()
        .find(|l| l.starts_with("a=rtpmap") && !l.contains("rtx") && !l.contains("red") && !l.contains("ulpfec"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.split('/').next())
        .unwrap_or("(unknown)")
        .to_owned();
    println!("\nNegotiated codec: {negotiated_codec}");

    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");

    // ── receiver: FrameTransform to capture encoded bytes ────────────────────

    let received_count = Arc::new(AtomicU32::new(0));
    let received_key   = Arc::new(AtomicU32::new(0));

    let recv_tf = FrameTransform::new({
        let received_count = received_count.clone();
        let received_key   = received_key.clone();
        move |f| {
            if f.data.is_empty() { return FrameAction::Drop; }

            println!(
                "  [recv] codec={} key={} size={}B ts={}",
                f.mime_type, f.is_key_frame, f.data.len(), f.timestamp,
            );

            // Here you would:
            //   - write f.data to a file/ring buffer  (recording)
            //   - push to another PeerConnection      (SFU forwarding)
            //   - feed a hardware decoder             (VideoToolbox / MediaCodec)
            //
            // FrameAction::Forward hands the bytes to the builtin decoder.
            // FrameAction::Drop discards them (right when using the null decoder
            // that comes with with_encoded_video_track).

            received_count.fetch_add(1, Ordering::SeqCst);
            if f.is_key_frame { received_key.fetch_add(1, Ordering::SeqCst); }
            FrameAction::Drop
        }
    });

    let rx2 = pc2
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc2 video transceiver");
    rx2.set_receiver_transform(&recv_tf).expect("receiver transform");

    // ── push pre-encoded frames ───────────────────────────────────────────────
    //
    // This is the developer-facing API. Whenever YOUR encoder (VideoToolbox,
    // NVENC, GStreamer, libvpx, rav1e, …) produces a frame, call:
    //
    //   video.push_encoded_frame(EncodedVideoFrame { data, is_key_frame, … });
    //
    // No raw pixel pumping. No closure. Any thread. Any rate.

    let encoded_count = Arc::new(AtomicU32::new(0));
    let stop          = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            // Simulate a hardware encoder running at 30 fps.
            // In production: replace this loop body with the output of your
            // encoder (VideoToolbox CMSampleBuffer, GstBuffer, AVPacket, …).
            let mut frame_idx = 0u32;
            while !stop.load(Ordering::SeqCst) {
                let is_key = frame_idx == 0 || frame_idx % 30 == 0;

                // ── stub encoded payload ──────────────────────────────────
                // Replace this with real encoder output:
                //   H264 → Annex-B NAL units (0x00 0x00 0x00 0x01 …)
                //   VP8  → raw VP8 bitstream
                //   VP9  → raw VP9 bitstream
                //   AV1  → OBU sequence
                //   H265 → Annex-B HEVC NAL units
                let mut data = vec![if is_key { 0xAA } else { 0xBB }; 64];
                data[1..5].copy_from_slice(&frame_idx.to_be_bytes());

                video.push_encoded_frame(EncodedVideoFrame {
                    data,
                    is_key_frame: is_key,
                    // 0 = inherit from the track's configured resolution (640×480).
                    width: 0,
                    height: 0,
                    rtp_timestamp: 0,
                });

                encoded_count.fetch_add(1, Ordering::SeqCst);
                frame_idx += 1;
                thread::sleep(Duration::from_millis(33)); // ~30 fps
            }
        });

        println!("\nWaiting for ICE connection and encoded frame delivery…\n");
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = encoded_count.load(Ordering::SeqCst) >= 5
                && received_count.load(Ordering::SeqCst) >= 3;
            if done || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let enc = encoded_count.load(Ordering::SeqCst);
    let rec = received_count.load(Ordering::SeqCst);
    let key = received_key.load(Ordering::SeqCst);
    let connected =
        s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);

    drop(recv_tf);

    if connected && enc > 0 && rec > 0 {
        println!(
            "\npre_encoded ✅ — pushed {enc} frame(s), \
             received {rec} ({key} key)"
        );
    } else {
        eprintln!(
            "\npre_encoded ❌ — connected={connected} pushed={enc} received={rec}"
        );
        std::process::exit(1);
    }
}
