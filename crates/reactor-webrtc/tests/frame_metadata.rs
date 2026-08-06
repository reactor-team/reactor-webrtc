//! End-to-end tests for per-frame metadata (packet trailer).
//!
//! Requires a native libwebrtc (see build.rs):
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc --test frame_metadata -- --nocapture
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    EncodedVideoFrame, FrameAction, FrameDirection, FrameMetadata, FrameTransform, IceCandidate,
    MediaKind, PeerConnection, PeerConnectionFactory, PeerConnectionObserver, PeerConnectionState,
    RtcConfiguration, Track, TransceiverDirection,
};

// ── Shared test plumbing (mirrors the pattern in encoded_transform.rs) ────────

#[derive(Default)]
struct Ice {
    q: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
    recv: Mutex<Vec<Track>>,
}

fn make_peer(
    factory: &PeerConnectionFactory,
    config: &RtcConfiguration,
) -> (PeerConnection, Arc<Ice>) {
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
        })
        .on_track({
            let s = ice.clone();
            move |_kind, track| {
                s.recv.lock().unwrap().push(track);
            }
        });
    let pc = factory
        .create_peer_connection(config, observer)
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

/// Run the offer/answer exchange between two peers.
///
/// Nothing here mentions frame metadata: `create_offer` advertises it and
/// `create_answer` mirrors it. The same declared bytes go to
/// `set_local_description`, so this also covers libwebrtc accepting our unknown
/// extmap URI in a *local* description.
fn negotiate(pc1: &PeerConnection, pc2: &PeerConnection) {
    let offer = pc1.create_offer().expect("offer");
    assert!(
        offer.declares_frame_metadata(),
        "create_offer must always advertise frame-metadata support"
    );
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");

    let answer = pc2.create_answer().expect("answer");
    assert_eq!(
        answer.frame_metadata_id(),
        offer.frame_metadata_id(),
        "the answer must echo the offer's extmap id"
    );
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");
}

/// Run the exchange with a peer that does not understand frame metadata.
///
/// `pc2` is still a reactor-webrtc connection, so its answer would normally
/// mirror ours; stripping the declaration from what it receives makes it behave
/// like a peer built before the capability existed.
fn negotiate_with_legacy_peer(pc1: &PeerConnection, pc2: &PeerConnection) {
    let offer = pc1.create_offer().expect("offer");
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&strip_frame_metadata(&offer))
        .expect("pc2 remote");

    let answer = pc2.create_answer().expect("answer");
    assert!(
        !answer.declares_frame_metadata(),
        "an offer with no declaration must produce an answer with none"
    );
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");
}

/// Drop every `a=extmap` line carrying our URI.
fn strip_frame_metadata(
    sdp: &reactor_webrtc::SessionDescription,
) -> reactor_webrtc::SessionDescription {
    let kept: String = sdp
        .sdp
        .lines()
        .filter(|l| !l.contains(reactor_webrtc::FRAME_METADATA_URI))
        .map(|l| format!("{l}\r\n"))
        .collect();
    reactor_webrtc::SessionDescription {
        kind: sdp.kind,
        sdp: kept,
    }
}

const W: u32 = 320;
const H: u32 = 240;

fn varying_bgra(seed: u8) -> Vec<u8> {
    let mut buf = vec![0x20u8; (W * H * 4) as usize];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(seed);
    }
    buf
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Sender pushes frames with `user_data`; receiver transform strips the
/// trailer and delivers `FrameMetadata` via `VideoFrame::metadata`.
/// Verifies `user_data`, monotonically-increasing `frame_id`, and a non-zero
/// `timestamp`.
#[test]
fn frame_metadata_roundtrip() {
    let factory = PeerConnectionFactory::new().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory
        .create_video_track("meta-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    // Nothing is attached by hand: negotiate() advertises the capability, the
    // answer mirrors it, and set_remote_description installs both transforms.
    negotiate(&pc1, &pc2);
    assert!(pc1.frame_metadata_gate().is_open());

    let received: Arc<Mutex<Vec<FrameMetadata>>> = Arc::new(Mutex::new(Vec::new()));
    let user_data: &[u8] = b"reactor-meta-e2e";
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let mut seed = 0u8;
            while !stop.load(Ordering::SeqCst) {
                let bgra = varying_bgra(seed);
                video.push_video_frame_with_metadata(&bgra, W, H, user_data);
                seed = seed.wrapping_add(7);
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        let mut recv_setup = false;

        loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);

            // Wire up the receiver once the remote track arrives via on_track.
            if !recv_setup {
                let mut tracks = s2.recv.lock().unwrap();
                if let Some(track) = tracks.iter_mut().find(|t| t.kind() == MediaKind::Video) {
                    let out = received.clone();
                    track.on_video_frame(move |frame| {
                        if let Some(meta) = frame.metadata {
                            out.lock().unwrap().push(meta);
                        }
                    });

                    recv_setup = true;
                }
            }

            if received.lock().unwrap().len() >= 3 || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let metas = received.lock().unwrap().clone();
    assert!(!metas.is_empty(), "no metadata received within 20 s");

    for (i, meta) in metas.iter().enumerate() {
        assert_eq!(
            meta.user_data, user_data,
            "user_data mismatch on sample {i}"
        );
        assert!(meta.frame_id > 0, "frame_id must be non-zero (sample {i})");
        assert!(
            meta.timestamp > 0,
            "timestamp must be non-zero (sample {i})"
        );
    }

    let ids: Vec<u64> = metas.iter().map(|m| m.frame_id).collect();
    for w in ids.windows(2) {
        assert!(
            w[1] > w[0],
            "frame_ids not monotonically increasing: {ids:?}"
        );
    }

    println!(
        "frame_metadata_roundtrip ✅  — {} metadata frames received",
        metas.len()
    );
}

/// Full E2E for `EncodedVideoTrack::push_encoded_frame_with_metadata`.
///
/// Phase 1: capture a real VP8 key frame from a standard BGRA loopback via a
/// sender FrameTransform.  Phase 2: replay those bytes via an `EncodedVideoTrack`
/// with `sender_metadata_transform` attached; verify that the receiver's
/// `on_video_frame` delivers `FrameMetadata` with the expected `user_data`,
/// non-zero `frame_id`, and monotonically increasing `frame_id`s.
#[test]
fn encoded_frame_metadata_roundtrip() {
    let config = RtcConfiguration::default();

    // ── Phase 1: capture a VP8 key frame ─────────────────────────────────────
    let kf_bytes: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    {
        let factory = PeerConnectionFactory::new().expect("factory");
        let (pc1, s1) = make_peer(&factory, &config);
        let (pc2, s2) = make_peer(&factory, &config);

        let tx1 = pc1
            .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
            .expect("tx");
        let video = factory
            .create_video_track("cap-video")
            .expect("video track");
        tx1.set_track(&video).expect("set track");

        let kf = kf_bytes.clone();
        let cap_tf = FrameTransform::new(move |frame| {
            if frame.direction == FrameDirection::Send && frame.is_key_frame {
                let mut g = kf.lock().unwrap();
                if g.is_none() {
                    *g = Some(frame.data.to_vec());
                }
            }
            FrameAction::Forward
        });
        tx1.set_sender_transform(&cap_tf)
            .expect("capture transform");

        // No metadata in this phase — just capturing encoded bytes.
        negotiate(&pc1, &pc2);

        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut seed = 0u8;
                while !stop.load(Ordering::SeqCst) {
                    let bgra = varying_bgra(seed);
                    video.push_video_frame(&bgra, W, H);
                    seed = seed.wrapping_add(7);
                    thread::sleep(Duration::from_millis(33));
                }
            });
            let start = Instant::now();
            loop {
                forward_ice(&s1, &pc2);
                forward_ice(&s2, &pc1);
                if kf_bytes.lock().unwrap().is_some() || start.elapsed() > Duration::from_secs(10) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            stop.store(true, Ordering::SeqCst);
        });
        drop(cap_tf);
    } // pc1, pc2, video drop here; factory drops here

    let vp8_bytes = kf_bytes
        .lock()
        .unwrap()
        .clone()
        .expect("no VP8 key frame captured in phase 1");

    // ── Phase 2: replay with metadata via EncodedVideoTrack ──────────────────
    let (factory2, enc_track) =
        PeerConnectionFactory::with_encoded_video_track("enc-meta", W, H).expect("factory2");

    let (pc3, s3) = make_peer(&factory2, &config);
    let (pc4, s4) = make_peer(&factory2, &config);

    let tx3 = pc3
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("tx3");
    tx3.set_track(enc_track.track()).expect("set track");

    // The FIFO source EncodedVideoTrack registered for this native track is what
    // the installed transform finds — no wiring here.
    negotiate(&pc3, &pc4);

    let received: Arc<Mutex<Vec<FrameMetadata>>> = Arc::new(Mutex::new(Vec::new()));
    let user_data: &[u8] = b"enc-meta-e2e";
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                enc_track.push_encoded_frame_with_metadata(
                    EncodedVideoFrame {
                        data: vp8_bytes.clone(),
                        is_key_frame: true,
                        width: W,
                        height: H,
                        rtp_timestamp: 0,
                    },
                    user_data,
                );
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        let mut recv_setup = false;

        loop {
            forward_ice(&s3, &pc4);
            forward_ice(&s4, &pc3);

            if !recv_setup {
                let mut tracks = s4.recv.lock().unwrap();
                if let Some(track) = tracks.iter_mut().find(|t| t.kind() == MediaKind::Video) {
                    let out = received.clone();
                    track.on_video_frame(move |frame| {
                        if let Some(meta) = frame.metadata {
                            out.lock().unwrap().push(meta);
                        }
                    });
                    recv_setup = true;
                }
            }

            if received.lock().unwrap().len() >= 3 || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let metas = received.lock().unwrap().clone();
    assert!(!metas.is_empty(), "no metadata received within 20 s");

    for (i, meta) in metas.iter().enumerate() {
        assert_eq!(
            meta.user_data, user_data,
            "user_data mismatch on sample {i}"
        );
        assert!(meta.frame_id > 0, "frame_id must be non-zero (sample {i})");
        assert!(
            meta.timestamp > 0,
            "timestamp must be non-zero (sample {i})"
        );
    }

    let ids: Vec<u64> = metas.iter().map(|m| m.frame_id).collect();
    for w in ids.windows(2) {
        assert!(
            w[1] > w[0],
            "frame_ids not monotonically increasing: {ids:?}"
        );
    }

    println!(
        "encoded_frame_metadata_roundtrip ✅  — {} metadata frames received",
        metas.len()
    );
}

/// Against a peer that never declares the capability, no trailer reaches the wire
/// at all — asserted on the encoded bytes, not on the absence of decoded metadata.
///
/// This is what the gate is for: `push_video_frame_with_metadata` keeps being
/// called, and the push site has no way to know the far end would not strip. The
/// receiver here installs its own `FrameTransform` to inspect the payload, which
/// is also how the assertion stays honest — with no declaration there is no strip
/// transform, so "metadata is None" would be true whether or not a trailer was
/// appended.
#[test]
fn legacy_peer_gets_no_trailer() {
    let factory = PeerConnectionFactory::new().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory
        .create_video_track("legacy-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    negotiate_with_legacy_peer(&pc1, &pc2);
    assert!(
        !pc1.frame_metadata_gate().is_open(),
        "gate must stay closed when the answer does not declare support"
    );

    let rx2 = pc2
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc2 video transceiver");

    let seen = Arc::new(Mutex::new(0usize));
    let trailers = Arc::new(Mutex::new(0usize));
    let inspect = FrameTransform::new({
        let seen = seen.clone();
        let trailers = trailers.clone();
        move |frame| {
            if frame.direction == FrameDirection::Receive {
                *seen.lock().unwrap() += 1;
                if frame.data.ends_with(b"RXMT") {
                    *trailers.lock().unwrap() += 1;
                }
            }
            FrameAction::Forward
        }
    });
    rx2.set_receiver_transform(&inspect)
        .expect("inspect transform");

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut seed = 0u8;
            while !stop.load(Ordering::SeqCst) {
                let bgra = varying_bgra(seed);
                video.push_video_frame_with_metadata(&bgra, W, H, b"must-not-ship");
                seed = seed.wrapping_add(7);
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            if *seen.lock().unwrap() >= 5 || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let frames = *seen.lock().unwrap();
    let tagged = *trailers.lock().unwrap();
    assert!(frames > 0, "no encoded frames arrived within 20 s");
    assert_eq!(
        tagged, 0,
        "{tagged} of {frames} encoded frames carried a trailer with the gate closed"
    );

    println!("legacy_peer_gets_no_trailer ✅  — {frames} frames, 0 trailers");
}

/// A caller's own sender transform and the metadata trailer work on the same
/// transceiver.
///
/// libwebrtc gives a sender one `SetFrameTransformer` slot, so this is only
/// expressible because the crate owns that slot and composes: the caller's callback
/// runs first, on the bytes the encoder produced, and the trailer is appended after.
/// Both observations are checked — the callback ran, *and* the receiver decoded
/// metadata.
#[test]
fn caller_transform_and_metadata_compose_on_one_sender() {
    let factory = PeerConnectionFactory::new().expect("factory");
    let config = RtcConfiguration::default();

    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    let tx1 = pc1
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("send transceiver");
    let video = factory
        .create_video_track("composed-video")
        .expect("video track");
    tx1.set_track(&video).expect("set track");

    // What the caller sees must be the encoder's output, with no trailer yet.
    let ran = Arc::new(AtomicBool::new(false));
    let saw_trailer = Arc::new(AtomicBool::new(false));
    let mine = FrameTransform::new({
        let ran = ran.clone();
        let saw_trailer = saw_trailer.clone();
        move |frame| {
            if frame.direction == FrameDirection::Send {
                ran.store(true, Ordering::SeqCst);
                if frame.data.ends_with(b"RXMT") {
                    saw_trailer.store(true, Ordering::SeqCst);
                }
            }
            FrameAction::Forward
        }
    });
    tx1.set_sender_transform(&mine).expect("my transform");

    negotiate(&pc1, &pc2);
    assert!(pc1.frame_metadata_gate().is_open());

    let received: Arc<Mutex<Vec<FrameMetadata>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let mut seed = 0u8;
            while !stop.load(Ordering::SeqCst) {
                let bgra = varying_bgra(seed);
                video.push_video_frame_with_metadata(&bgra, W, H, b"composed");
                seed = seed.wrapping_add(7);
                thread::sleep(Duration::from_millis(33));
            }
        });

        let start = Instant::now();
        let mut recv_setup = false;
        loop {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            if !recv_setup {
                let mut tracks = s2.recv.lock().unwrap();
                if let Some(track) = tracks.iter_mut().find(|t| t.kind() == MediaKind::Video) {
                    let out = received.clone();
                    track.on_video_frame(move |frame| {
                        if let Some(meta) = frame.metadata {
                            out.lock().unwrap().push(meta);
                        }
                    });
                    recv_setup = true;
                }
            }
            if received.lock().unwrap().len() >= 2 || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    assert!(
        ran.load(Ordering::SeqCst),
        "the caller's sender transform never ran"
    );
    assert!(
        !saw_trailer.load(Ordering::SeqCst),
        "the caller's transform saw a trailer — it must run before the metadata step"
    );
    let metas = received.lock().unwrap().clone();
    assert!(
        !metas.is_empty(),
        "no metadata arrived — composing dropped the metadata step"
    );
    assert_eq!(metas[0].user_data, b"composed");

    println!(
        "caller_transform_and_metadata_compose_on_one_sender ✅  — {} metadata frames",
        metas.len()
    );
}

/// `RtcConfiguration::frame_metadata = false` keeps the capability out of the SDP
/// and off the wire, on a connection that is otherwise identical.
#[test]
fn disabled_frame_metadata_never_negotiates() {
    let factory = PeerConnectionFactory::new().expect("factory");
    let off = RtcConfiguration {
        frame_metadata: false,
        ..Default::default()
    };
    let on = RtcConfiguration::default();

    // Offerer disabled: it does not advertise, so the answerer has nothing to
    // mirror even though the answerer has the capability on.
    let (pc1, _s1) = make_peer(&factory, &off);
    let (pc2, _s2) = make_peer(&factory, &on);
    pc1.add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("tx");

    let offer = pc1.create_offer().expect("offer");
    assert!(
        !offer.declares_frame_metadata(),
        "a disabled connection must not advertise the capability"
    );
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");
    let answer = pc2.create_answer().expect("answer");
    assert!(!answer.declares_frame_metadata());
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");
    assert!(!pc1.frame_metadata_gate().is_open());
    assert!(!pc2.frame_metadata_gate().is_open());

    // Answerer disabled: the offer declares, but it must not mirror.
    let (pc3, _s3) = make_peer(&factory, &on);
    let (pc4, _s4) = make_peer(&factory, &off);
    pc3.add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)
        .expect("tx");

    let offer = pc3.create_offer().expect("offer3");
    assert!(offer.declares_frame_metadata());
    pc3.set_local_description(&offer).expect("pc3 local");
    pc4.set_remote_description(&offer).expect("pc4 remote");
    let answer = pc4.create_answer().expect("answer3");
    assert!(
        !answer.declares_frame_metadata(),
        "a disabled answerer must not mirror the offer's declaration"
    );
    assert!(!pc4.frame_metadata_gate().is_open());
    pc4.set_local_description(&answer).expect("pc4 local");
    pc3.set_remote_description(&answer).expect("pc3 remote");
    assert!(
        !pc3.frame_metadata_gate().is_open(),
        "an unmirrored offer must leave the offerer's gate shut"
    );

    println!("disabled_frame_metadata_never_negotiates ✅");
}
