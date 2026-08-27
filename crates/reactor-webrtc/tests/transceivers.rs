//! Transceiver directions + mid mapping + ICE-gathering callback + the msid a
//! published track carries — the pieces the SDK peer transport needs (recvonly
//! to receive, sendonly to publish, mid per m-section, one MediaStream so the
//! remote can sync audio against video, and end-of-gathering).
#![cfg(have_libwebrtc)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    AudioTrackOptions, AudioTrackSource, IceGatheringState, MediaKind, PeerConnectionFactory,
    PeerConnectionObserver, RtcConfiguration, TransceiverDirection,
};

fn wait_for(p: impl Fn() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    p()
}

#[test]
fn transceivers_mid_and_ice_gathering() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");

    let gathering_complete = Arc::new(AtomicBool::new(false));
    let observer = PeerConnectionObserver::new().on_ice_gathering_change({
        let g = gathering_complete.clone();
        move |state| {
            if state == IceGatheringState::Complete {
                g.store(true, Ordering::SeqCst);
            }
        }
    });
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), observer)
        .expect("pc");

    // recvonly video (receive a remote track) + sendonly audio (publish a track).
    let vrecv = pc
        .add_transceiver(MediaKind::Video, TransceiverDirection::RecvOnly)
        .expect("add recvonly video");
    let asend = pc
        .add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly)
        .expect("add sendonly audio");
    let mic = factory.create_audio_track("mic").expect("audio track");
    asend
        .set_track(&mic)
        .expect("attach track to sendonly transceiver");

    // mids are unassigned until set_local_description.
    assert!(vrecv.mid().is_none(), "mid should be None before SLD");

    let offer = pc.create_offer().expect("offer");
    assert!(offer.sdp.contains("m=video"), "offer has a video m-line");
    assert!(offer.sdp.contains("m=audio"), "offer has an audio m-line");
    assert!(
        offer.sdp.contains("a=recvonly"),
        "recvonly transceiver yields a=recvonly (no SDP patching needed):\n{}",
        offer.sdp
    );
    pc.set_local_description(&offer).expect("set local");

    // mids are assigned after SLD and map each transceiver to its m-section.
    let vmid = vrecv.mid().expect("video mid after SLD");
    let amid = asend.mid().expect("audio mid after SLD");
    assert_ne!(vmid, amid, "distinct mids");

    // A lone PC gathers host candidates and reaches Complete.
    assert!(
        wait_for(
            || gathering_complete.load(Ordering::SeqCst),
            Duration::from_secs(5)
        ),
        "ICE gathering did not reach Complete",
    );

    println!(
        "transceivers OK — video mid={vmid} (recvonly), audio mid={amid} (sendonly); ICE gathering complete"
    );

    drop((vrecv, asend, mic));
    drop(pc);
    drop(factory);
}

/// An answerer that publishes over transceivers the remote offer created — the
/// SDK peer's flow — groups its audio and video into one MediaStream, and leaves
/// the sections it only receives on untouched. Without a shared msid stream id
/// the remote has no sync group for the published pair and plays each out on its
/// own clock, which shows up as A/V drift.
#[test]
fn published_tracks_share_one_media_stream() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");

    // The bidirectional shape the SDK peer answers: the offerer sends a camera
    // and a mic (mids 0, 1) and asks to receive a processed pair (mids 2, 3), so
    // the answerer publishes on the second pair only.
    let offerer = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("offerer");
    for (kind, direction) in [
        (MediaKind::Video, TransceiverDirection::SendOnly),
        (MediaKind::Audio, TransceiverDirection::SendOnly),
        (MediaKind::Video, TransceiverDirection::RecvOnly),
        (MediaKind::Audio, TransceiverDirection::RecvOnly),
    ] {
        offerer
            .add_transceiver(kind, direction)
            .expect("offerer transceiver");
    }
    let offer = offerer.create_offer().expect("offer");
    offerer.set_local_description(&offer).expect("offerer SLD");

    let answerer = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("answerer");
    answerer
        .set_remote_description(&offer)
        .expect("answerer SRD");

    let video = factory
        .create_video_track("out_video")
        .expect("video track");
    let audio = factory
        .create_audio_track_with_options("out_audio", {
            let mut options = AudioTrackOptions::default();
            options.source = AudioTrackSource::LocalPush;
            options
        })
        .expect("audio track");
    // Publish by mid, the way the peer resolves a client's track mapping: the
    // mids the offer marked recvonly get a track, the rest stay receive-only.
    let published = ["2", "3"];
    for tc in answerer.transceivers() {
        let mid = tc.mid().expect("mid assigned by SRD");
        if !published.contains(&mid.as_str()) {
            continue;
        }
        let track = match tc.kind() {
            MediaKind::Video => &video,
            MediaKind::Audio => &audio,
            MediaKind::Unknown => panic!("transceiver of unknown kind"),
        };
        tc.set_track(track).expect("attach track");
        tc.set_direction(TransceiverDirection::SendOnly)
            .expect("publish sendonly");
    }

    let answer = answerer.create_answer().expect("answer");
    assert_eq!(
        answer.sdp.matches("a=recvonly").count(),
        2,
        "the two sections the answerer only receives on:\n{}",
        answer.sdp
    );
    assert_eq!(
        answer.sdp.matches("a=sendonly").count(),
        2,
        "the two sections the answerer publishes on:\n{}",
        answer.sdp
    );

    // msid describes a sending track, so only the published pair carries one —
    // a receive-only section has nothing to name, whatever its sender holds.
    let msids: Vec<&str> = answer
        .sdp
        .lines()
        .filter_map(|l| l.strip_prefix("a=msid:"))
        .collect();
    assert_eq!(
        msids.len(),
        2,
        "one msid per published track, none for the received pair:\n{}",
        answer.sdp
    );
    for msid in &msids {
        let stream_id = msid.split_whitespace().next().expect("msid stream id");
        assert_eq!(
            stream_id, "reactor-stream",
            "published track must name a real stream, not '-':\n{}",
            answer.sdp
        );
    }

    println!("msid OK — both published tracks in one MediaStream: {msids:?}");

    drop((video, audio));
    drop((offerer, answerer));
    drop(factory);
}
