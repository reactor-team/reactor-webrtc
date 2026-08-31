//! `set_codec_preferences` alone is not enough for an **answerer**'s own
//! sender: it reorders the SDP, but the sender still encodes with whatever
//! it would have picked anyway (the remote offer's own codec order), unless
//! something also locks the sender onto the preferred codec once
//! negotiation completes. `PeerConnection::set_local_description` does that
//! automatically for every video transceiver with a preference set — this
//! test proves the answerer's sender ends up encoding with the preferred
//! codec without any call beyond `set_codec_preferences` and the normal
//! signaling sequence. Verified against the actual encoded bitstream's
//! `mime_type`, not just the SDP text.
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

/// Remove every trace of `codec_name` from an SDP's `m=video` section: its
/// `rtpmap`/`fmtp`/`rtcp-fb` lines, its payload type in the `m=video` line,
/// and any associated RTX payload type (`a=fmtp:<rtx-pt> apt=<pt>`) along
/// with its own lines. Simulates a remote peer that never offered the codec
/// at all, as opposed to `set_codec_preferences` merely not preferring it.
fn strip_video_codec(sdp: &str, codec_name: &str) -> String {
    let rtpmap_marker = format!(" {codec_name}/");
    let mut dropped_pts: Vec<String> = sdp
        .lines()
        .filter(|l| l.starts_with("a=rtpmap:") && l.contains(&rtpmap_marker))
        .filter_map(|l| l.strip_prefix("a=rtpmap:")?.split_whitespace().next())
        .map(str::to_string)
        .collect();
    // The RTX entry for a dropped codec carries its own payload type, linked
    // back via "a=fmtp:<rtx-pt> apt=<pt>" — drop that pt too, or the m=video
    // line would list an RTX payload type with nothing left for it to retransmit.
    let apt_rtx_pts: Vec<String> = sdp
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("a=fmtp:")?;
            let (pt, params) = rest.split_once(' ')?;
            let apt = params.strip_prefix("apt=")?;
            dropped_pts
                .contains(&apt.to_string())
                .then(|| pt.to_string())
        })
        .collect();
    dropped_pts.extend(apt_rtx_pts);

    sdp.lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("m=video ") {
                let mut fields: Vec<&str> = rest.split(' ').collect();
                // "<port> <proto> <pt> <pt> ...": payload types start at index 2.
                let head = fields.drain(..2).collect::<Vec<_>>();
                fields.retain(|pt| !dropped_pts.iter().any(|d| d == pt));
                format!(
                    "m=video {}",
                    head.into_iter().chain(fields).collect::<Vec<_>>().join(" ")
                )
            } else {
                line.to_string()
            }
        })
        .filter(|line| {
            !["a=rtpmap:", "a=fmtp:", "a=rtcp-fb:"].iter().any(|prefix| {
                line.strip_prefix(prefix).is_some_and(|rest| {
                    let pt = rest
                        .split(|c: char| c == ' ' || c == ':')
                        .next()
                        .unwrap_or("");
                    dropped_pts.iter().any(|d| d == pt)
                })
            })
        })
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
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
fn set_local_description_locks_the_answerer_send_codec_automatically() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");

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
    // codec_preferences.rs). `pc2.set_local_description(&answer)` above is
    // what should have also locked the sender itself onto that codec,
    // with no further call needed.
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
                track
                    .push_frame(reactor_webrtc::VideoFrame::new(&bgra, w, h))
                    .expect("push frame");
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

/// The top preference can be configured but never actually negotiated with
/// this peer (unsupported on their end, or simply not offered) — the lock
/// must then fall through to the next-preferred codec that *was* negotiated,
/// rather than giving up because the first choice alone did not match.
#[test]
fn falls_back_to_the_next_preferred_codec_when_the_top_choice_was_not_negotiated() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");

    // pc1 stands in for a browser that never offers VP9 at all — as opposed
    // to set_codec_preferences on pc2 simply not asking for it.
    let (pc1, s1) = make_peer(&factory);
    let (pc2, s2) = make_peer(&factory);

    pc1.add_transceiver(MediaKind::Video, TransceiverDirection::RecvOnly)
        .expect("pc1 recvonly video");
    let offer = pc1.create_offer().expect("offer");
    let offer_sdp = strip_video_codec(&offer.sdp, "VP9");
    assert!(
        !offer_sdp.contains("VP9"),
        "VP9 should be fully stripped from the offer:\n{offer_sdp}"
    );
    let offer = reactor_webrtc::SessionDescription {
        sdp: offer_sdp,
        ..offer
    };
    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");

    let tx2 = pc2
        .transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Video)
        .expect("pc2 video transceiver");
    let track = factory.create_video_track("cam").expect("video track");
    tx2.set_track(&track).expect("set track");
    tx2.set_direction(TransceiverDirection::SendOnly)
        .expect("set direction");
    // VP9 first even though the remote never offered it: set_codec_preferences
    // only checks local support, not what the other side will negotiate. AV1,
    // not VP8, is the fallback: VP8 is libwebrtc's own default pick once VP9
    // is unavailable, so preferring it here would pass even with the bug —
    // AV1 only ends up as the send codec if the fallback loop actually ran.
    tx2.set_codec_preferences(&[VideoCodec::Vp9, VideoCodec::Av1])
        .expect("set codec preferences");

    let answer = pc2.create_answer().expect("answer");
    assert!(
        !answer.sdp.contains("VP9"),
        "the answer cannot negotiate a codec the offer never listed:\n{}",
        answer.sdp
    );
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

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
                track
                    .push_frame(reactor_webrtc::VideoFrame::new(&bgra, w, h))
                    .expect("push frame");
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
        mime, "video/AV1",
        "should fall back to AV1 (the next preference that was actually negotiated), \
         not silently give up because VP9 alone was not found"
    );

    drop(send_tf);
}
