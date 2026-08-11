//! `Transceiver::lock_negotiated_send_codec` — proves `set_codec_preferences`
//! alone is not enough for an **answerer**'s own sender: it reorders the SDP,
//! but the sender still encodes with whatever it would have picked anyway
//! (the remote offer's own codec order) until `lock_negotiated_send_codec`
//! is also called after negotiation completes. Verified against the actual
//! encoded bitstream's `mime_type`, not just the SDP text.
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    FrameAction, FrameTransform, IceCandidate, MediaKind, PeerConnection, PeerConnectionFactory,
    PeerConnectionObserver, RtcConfiguration, TransceiverDirection, VideoCodec,
};

#[derive(Default)]
struct Ice {
    q: Mutex<VecDeque<IceCandidate>>,
}

fn make_peer(factory: &PeerConnectionFactory) -> (PeerConnection, Arc<Ice>) {
    let ice = Arc::new(Ice::default());
    let observer = PeerConnectionObserver::new().on_ice_candidate({
        let s = ice.clone();
        move |c| s.q.lock().unwrap().push_back(c)
    });
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), observer)
        .expect("create peer connection");
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

/// The codec name carried by the first payload type on the offer's `m=video`
/// line — libwebrtc's own default preference, with nothing configured.
fn default_first_video_codec(factory: &PeerConnectionFactory) -> String {
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("pc");
    pc.add_transceiver(MediaKind::Video, TransceiverDirection::SendRecv)
        .expect("add video transceiver");
    let offer = pc.create_offer().expect("offer");
    let video_line = offer
        .sdp
        .lines()
        .find(|l| l.starts_with("m=video"))
        .expect("m=video line");
    let first_pt = video_line
        .split_whitespace()
        .nth(3)
        .expect("a payload type");
    let rtpmap_prefix = format!("a=rtpmap:{first_pt} ");
    let rtpmap = offer
        .sdp
        .lines()
        .find(|l| l.starts_with(&rtpmap_prefix))
        .expect("rtpmap for the first payload type");
    rtpmap[rtpmap_prefix.len()..]
        .split('/')
        .next()
        .expect("codec/clock-rate subtype")
        .to_string()
}

#[test]
fn lock_negotiated_send_codec_makes_the_answerer_actually_encode_with_it() {
    let factory = PeerConnectionFactory::new().expect("factory");

    let default_first = default_first_video_codec(&factory);
    let (preferred, preferred_name) = if default_first.eq_ignore_ascii_case("VP9") {
        (VideoCodec::Vp8, "VP8")
    } else {
        (VideoCodec::Vp9, "VP9")
    };

    // pc1 stands in for a browser client: it offers recvonly video, i.e. it
    // wants pc2 (the answerer, standing in for reactor-runtime) to send.
    let (pc1, s1) = make_peer(&factory);
    let (pc2, s2) = make_peer(&factory);

    pc1.add_transceiver(MediaKind::Video, TransceiverDirection::RecvOnly)
        .expect("pc1 recvonly video");
    let offer = pc1.create_offer().expect("offer");
    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");

    // pc2's side of the m-section is auto-created from the offer. Attach a
    // track, flip it to sendonly, and ask for our non-default codec — exactly
    // what reactor-runtime's _attach_out_tracks does for an OUT track.
    let tx2 = pc2
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc2 video transceiver");
    let track = factory.create_video_track("cam").expect("video track");
    tx2.set_track(&track).expect("set track");
    tx2.set_direction(TransceiverDirection::SendOnly)
        .expect("set direction");
    tx2.set_codec_preferences(&[preferred])
        .expect("set codec preferences");

    let answer = pc2.create_answer().expect("answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    // The SDP already lists the preferred codec first (proven by
    // codec_preferences.rs); without lock_negotiated_send_codec, the sender
    // still encodes with whatever it would have picked anyway.
    tx2.lock_negotiated_send_codec()
        .expect("lock negotiated send codec");

    let seen_mime = Arc::new(Mutex::new(None::<String>));
    let send_tf = FrameTransform::new({
        let seen_mime = seen_mime.clone();
        move |f| {
            let mut seen = seen_mime.lock().unwrap();
            if seen.is_none() && !f.data.is_empty() {
                *seen = Some(f.mime_type.to_string());
            }
            FrameAction::Forward
        }
    });
    tx2.set_sender_transform(&send_tf)
        .expect("attach sender transform");

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let (w, h) = (320u32, 240u32);
            let mut bgra = vec![0x20u8; (w * h * 4) as usize];
            let mut t = 0u8;
            while !stop.load(Ordering::SeqCst) {
                for (i, b) in bgra.iter_mut().enumerate() {
                    *b = (i as u8).wrapping_add(t);
                }
                track.push_video_frame(&bgra, w, h);
                t = t.wrapping_add(7);
                thread::sleep(Duration::from_millis(30));
            }
        });

        let start = Instant::now();
        while seen_mime.lock().unwrap().is_none() && start.elapsed() < Duration::from_secs(15) {
            forward_ice(&s1, &pc2);
            forward_ice(&s2, &pc1);
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });

    let mime = seen_mime
        .lock()
        .unwrap()
        .clone()
        .expect("sender transform saw no encoded frame");
    assert_eq!(
        mime,
        format!("video/{preferred_name}"),
        "answerer's sender should encode with the preferred codec, not fall back to the \
         remote offer's own default ({default_first})"
    );

    drop(send_tf);
}
