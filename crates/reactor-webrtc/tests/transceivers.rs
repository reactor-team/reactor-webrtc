//! Transceiver directions + mid mapping + ICE-gathering callback — the pieces
//! the SDK peer transport needs (recvonly to receive, sendonly to publish, mid
//! per m-section, and end-of-gathering).
#![cfg(have_libwebrtc)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceGatheringState, MediaKind, PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration,
    TransceiverDirection,
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
    let factory = PeerConnectionFactory::new().expect("factory");

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
