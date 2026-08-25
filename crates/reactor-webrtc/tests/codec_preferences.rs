//! `Transceiver::set_codec_preferences` — the video m-line's payload-type
//! order changes to match the requested preference, without dropping any
//! codec the endpoint actually supports; the call is rejected on audio.
#![cfg(have_libwebrtc)]

use reactor_webrtc::{
    MediaKind, PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration,
    TransceiverDirection, VideoCodec,
};

/// The codec name (`rtpmap` subtype) carried by the first payload type listed
/// on the offer's `m=video` line — i.e. the most preferred negotiable codec.
fn first_video_codec(sdp: &str) -> String {
    let video_line = sdp
        .lines()
        .find(|l| l.starts_with("m=video"))
        .expect("offer has an m=video line");
    // "m=video <port> <proto> <pt> <pt> ...": payload types start at field 3.
    let first_pt = video_line
        .split_whitespace()
        .nth(3)
        .expect("m=video line lists at least one payload type");
    let rtpmap_prefix = format!("a=rtpmap:{first_pt} ");
    let rtpmap = sdp
        .lines()
        .find(|l| l.starts_with(&rtpmap_prefix))
        .unwrap_or_else(|| panic!("no rtpmap for payload type {first_pt}:\n{sdp}"));
    rtpmap[rtpmap_prefix.len()..]
        .split('/')
        .next()
        .expect("rtpmap has a codec/clock-rate subtype")
        .to_string()
}

#[test]
fn set_codec_preferences_reorders_without_dropping() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("pc");
    let tx = pc
        .add_transceiver(MediaKind::Video, TransceiverDirection::SendRecv)
        .expect("add video transceiver");

    let offer = pc.create_offer().expect("offer");
    let default_first = first_video_codec(&offer.sdp);

    // Ask for whichever builtin codec libwebrtc did *not* already put first,
    // so the assertion below proves set_codec_preferences actually did
    // something rather than observing the existing default order.
    let (preferred, preferred_name) = if default_first.eq_ignore_ascii_case("VP9") {
        (VideoCodec::Vp8, "VP8")
    } else {
        (VideoCodec::Vp9, "VP9")
    };
    tx.set_codec_preferences(&[preferred])
        .expect("set_codec_preferences on a video transceiver");

    let reordered = pc.create_offer().expect("offer after reordering");
    let new_first = first_video_codec(&reordered.sdp);
    assert!(
        new_first.eq_ignore_ascii_case(preferred_name),
        "expected {preferred_name} first after set_codec_preferences, got {new_first}:\n{}",
        reordered.sdp
    );

    // The codec that used to be first is reordered, not dropped: it still has
    // an rtpmap entry somewhere in the (possibly new) offer.
    assert!(
        reordered
            .sdp
            .lines()
            .any(|l| l.starts_with("a=rtpmap:") && l.contains(&format!(" {default_first}/"))),
        "{default_first} should still be offered, just no longer first:\n{}",
        reordered.sdp
    );
}

#[test]
fn set_codec_preferences_rejects_audio_transceiver() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("pc");
    let tx = pc
        .add_transceiver(MediaKind::Audio, TransceiverDirection::SendRecv)
        .expect("add audio transceiver");

    let err = tx
        .set_codec_preferences(&[VideoCodec::Vp8])
        .expect_err("set_codec_preferences must reject an audio transceiver");
    assert!(err.to_string().contains("set_codec_preferences"));
}
