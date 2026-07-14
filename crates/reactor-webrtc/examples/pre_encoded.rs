//! Send and receive pre-encoded video — bypassing libwebrtc's built-in codec.
//!
//! # Encoder side (`CustomVideoEncoder`)
//!
//! `PeerConnectionFactory::with_custom_video_encoder(encoder)` replaces every
//! software encoder in the factory with your closure. libwebrtc still handles
//! packetization, congestion control, RTCP, and ICE. You handle the pixels-to-
//! bitstream conversion.
//!
//! The closure is invoked **synchronously** on the encoder thread for every I420
//! frame. Return `Some(EncodedVideoFrame)` to inject encoded bytes into the RTP
//! stack, or `None` to silently drop the frame.
//!
//! `frame.codec` tells you which codec was negotiated so you can call the right
//! hardware encoder:
//!
//! | `VideoCodec` | Use with                                |
//! |--------------|-----------------------------------------|
//! | `Vp8`        | libvpx (`vpx_codec_encode`)             |
//! | `Vp9`        | libvpx (`vpx_codec_encode` VP9 profile) |
//! | `H264`       | VideoToolbox / MediaCodec / x264        |
//! | `Av1`        | libaom / SVT-AV1 / rav1e               |
//! | `H265`       | VideoToolbox / MediaCodec / libx265     |
//!
//! # Receiver side (`FrameTransform`)
//!
//! A `FrameTransform` attached to the receiver sits between the RTP
//! depacketizer and the decoder. Returning `FrameAction::Forward` hands the
//! encoded bytes to libwebrtc's decoder; returning `FrameAction::Drop` bypasses
//! it entirely — useful for SFU forwarding, recording, or when you use a
//! `CustomVideoEncoder` factory on the receiver side (whose null decoder would
//! discard the frame anyway).
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
            "Set REACTOR_WEBRTC_LIB_DIR=<path/to/dist> to link a native libwebrtc build.\n\
             See the README for download instructions."
        );
        return;
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
        CustomVideoEncoder, EncodedVideoFrame, FrameAction, FrameTransform, IceCandidate,
        MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
        PeerConnectionState, RtcConfiguration, RawVideoFrame, Track, TransceiverDirection,
        VideoCodec,
    };

    // ── stub encoder ──────────────────────────────────────────────────────────
    //
    // In production replace this with a call to your hardware encoder:
    //   VideoToolbox  →  VTCompressionSessionEncodeFrame
    //   MediaCodec    →  codec.queueInputBuffer / dequeueOutputBuffer
    //   GStreamer     →  appsrc ! encoder ! appsink
    //   libvpx        →  vpx_codec_encode
    //
    // The function must be synchronous for this closure-based API. If your
    // encoder is asynchronous, copy the I420 planes into your pipeline and
    // block (with a channel or condvar) until the encoded frame is ready.
    fn encode_frame(frame: &RawVideoFrame<'_>, frame_index: u32) -> Option<EncodedVideoFrame> {
        // Every 30th frame is a keyframe; the first frame is always a keyframe.
        let is_key = frame.request_key_frame || frame_index == 0 || frame_index % 30 == 0;

        // ── STUB — replace with real encoder output ────────────────────────
        // The bytes here are NOT a valid codec bitstream. They serve only to
        // demonstrate that an arbitrary payload travels through WebRTC's RTP
        // stack and arrives at the receiver's FrameTransform intact.
        //
        // When you swap in a real encoder, return the raw bitstream bytes:
        //   H264 → Annex-B (0x00 0x00 0x00 0x01 … NAL units)
        //   VP8  → raw VP8 bitstream (the RTP packetizer adds the descriptor)
        //   VP9  → raw VP9 bitstream
        //   AV1  → OBU sequence (the packetizer handles AV1-RTP framing)
        //   H265 → Annex-B HEVC bitstream
        let stub = {
            let mut v = Vec::with_capacity(64);
            // A tag byte that makes the payload easy to spot in logs.
            v.push(if is_key { 0xAA } else { 0xBB });
            v.push((frame.codec as u32 & 0xFF) as u8);
            v.extend_from_slice(&frame_index.to_be_bytes());
            v.extend_from_slice(&frame.rtp_timestamp.to_be_bytes());
            // Pad to 64 bytes so the RTP packetizer has something to work with.
            v.resize(64, 0x00);
            v
        };

        Some(EncodedVideoFrame {
            data: stub,
            is_key_frame: is_key,
            // 0 = inherit width / height / rtp_timestamp from the raw frame.
            width: 0,
            height: 0,
            rtp_timestamp: 0,
        })
    }

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

    // ── sender factory with custom encoder ───────────────────────────────────

    let encoded_count = Arc::new(AtomicU32::new(0));

    let encoder = CustomVideoEncoder::new({
        let encoded_count = encoded_count.clone();
        let frame_index   = Arc::new(AtomicU32::new(0));

        move |frame| {
            let idx = frame_index.fetch_add(1, Ordering::SeqCst);

            // `frame.codec` is the negotiated codec for this session.
            // You'd branch here to call the appropriate hardware encoder.
            match frame.codec {
                VideoCodec::Vp8  => {} // call libvpx for VP8
                VideoCodec::Vp9  => {} // call libvpx for VP9
                VideoCodec::H264 => {} // call VideoToolbox / x264 for H.264
                VideoCodec::Av1  => {} // call libaom / rav1e for AV1
                VideoCodec::H265 => {} // call VideoToolbox / libx265 for H.265
            }

            let result = encode_frame(frame, idx)?;
            encoded_count.fetch_add(1, Ordering::SeqCst);
            Some(result)
        }
    });

    // This factory replaces the built-in software encoder on both peers.
    // pc2 is receive-only, so its encoder callback never fires — only pc1's does.
    let factory =
        PeerConnectionFactory::with_custom_video_encoder(encoder).expect("factory");

    let config    = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    // ── sender: add a video transceiver ──────────────────────────────────────

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory
        .create_video_track("reactor-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    // ── offer / answer ────────────────────────────────────────────────────────

    let offer = pc1.create_offer().expect("create offer");

    // The SDP will list every codec the factory supports: VP8, VP9, H264, AV1, H265.
    // The answering peer picks the highest-preference common codec.
    println!("Offer codecs on m=video:");
    for line in offer.sdp.lines().filter(|l| l.starts_with("a=rtpmap")) {
        println!("  {line}");
    }

    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");

    let answer = pc2.create_answer().expect("create answer");
    println!("Answer codecs on m=video:");
    for line in answer.sdp.lines().filter(|l| l.starts_with("a=rtpmap")) {
        println!("  {line}");
    }

    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");

    // ── receiver: attach a FrameTransform ────────────────────────────────────
    //
    // The transform sits between the RTP depacketizer and the decoder.
    // FrameAction::Drop bypasses the decoder (the factory uses a null decoder
    // for codecs not in the builtin set, so this is the expected path).
    // FrameAction::Forward hands the frame to the decoder as normal.

    let received_count = Arc::new(AtomicU32::new(0));
    let received_key   = Arc::new(AtomicU32::new(0));

    let recv_tf = FrameTransform::new({
        let received_count = received_count.clone();
        let received_key   = received_key.clone();
        move |f| {
            if f.data.is_empty() {
                return FrameAction::Drop;
            }

            // The encoded bytes your custom encoder produced arrive here,
            // reconstructed from RTP. In production you would:
            //   - write them to a file or ring buffer (recording)
            //   - forward to another peer connection (SFU)
            //   - push into a hardware decoder (VideoToolbox, MediaCodec)
            let tag      = f.data[0];
            let is_key   = f.is_key_frame;
            let mime     = f.mime_type;
            let payload  = f.data.len();
            println!(
                "  [pc2 recv] mime={mime} key={is_key} tag=0x{tag:02X} payload={payload}B"
            );

            received_count.fetch_add(1, Ordering::SeqCst);
            if is_key {
                received_key.fetch_add(1, Ordering::SeqCst);
            }

            // Drop: the null decoder would discard the frame anyway.
            // Switch to Forward if you want libwebrtc's builtin decoder to run.
            FrameAction::Drop
        }
    });

    // The transceiver that pc2 auto-created from the offer.
    let rx2 = pc2
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc2 video transceiver");
    rx2.set_receiver_transform(&recv_tf)
        .expect("receiver transform");

    // ── media pump + ICE loop ─────────────────────────────────────────────────

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let mut bgra  = vec![0x20u8; (w * h * 4) as usize];
            let mut phase = 0u8;
            while !stop.load(Ordering::SeqCst) {
                // Vary the content so the encoder sees distinct frames.
                for (i, b) in bgra.iter_mut().enumerate() {
                    *b = ((i as u32 + phase as u32 * 7) % 256) as u8;
                }
                video.push_video_frame(&bgra, w, h);
                phase = phase.wrapping_add(1);
                thread::sleep(Duration::from_millis(30));
            }
        });

        println!("\nWaiting for ICE and encoded frame delivery…");
        let start = Instant::now();
        loop {
            trickle(&s1, &pc2);
            trickle(&s2, &pc1);
            let done = encoded_count.load(Ordering::SeqCst) >= 3
                && received_count.load(Ordering::SeqCst) >= 1;
            if done || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    // ── results ───────────────────────────────────────────────────────────────

    let enc = encoded_count.load(Ordering::SeqCst);
    let rec = received_count.load(Ordering::SeqCst);
    let key = received_key.load(Ordering::SeqCst);
    let connected =
        s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst);

    drop(recv_tf); // keep transform alive until here

    if connected && enc > 0 && rec > 0 {
        println!(
            "\npre_encoded ✅ — encoded {enc} frame(s), \
             received {rec} frame(s) ({key} key)"
        );
    } else {
        eprintln!(
            "\npre_encoded ❌ — connected={connected} encoded={enc} received={rec}"
        );
        std::process::exit(1);
    }
}
