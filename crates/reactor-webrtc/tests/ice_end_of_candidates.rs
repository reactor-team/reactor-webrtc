//! An empty candidate string is the end-of-candidates marker, not a parse error.
//!
//! RFC 8838 lets a trickle-ICE sender close out gathering with a candidate
//! whose candidate field is empty — the end-of-candidates marker. libwebrtc's
//! candidate parser rejects the empty string ("Expected candidate got "), so
//! whether negotiation survived depended on the order a client trickled its
//! candidates in. This test pins the contract the glue now guarantees: the
//! marker is accepted as a no-op, real candidates still apply, and garbage
//! still fails the parse.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test -p reactor-webrtc --test ice_end_of_candidates
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration,
};

#[derive(Default)]
struct Shared {
    ice: Mutex<VecDeque<IceCandidate>>,
}

fn make_peer(factory: &PeerConnectionFactory) -> (PeerConnection, Arc<Shared>) {
    let shared = Arc::new(Shared::default());
    let observer = PeerConnectionObserver::new().on_ice_candidate({
        let s = shared.clone();
        move |c| s.ice.lock().unwrap().push_back(c)
    });
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), observer)
        .expect("create peer connection");
    (pc, shared)
}

/// Reach the state trickle ICE happens in: both peers described, candidates
/// arriving on pc1's observer queue.
fn negotiate() -> (PeerConnection, PeerConnection, Arc<Shared>, Arc<Shared>) {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let (pc1, s1) = make_peer(&factory);
    let (pc2, s2) = make_peer(&factory);

    pc1.create_data_channel("probe").expect("data channel");
    let offer = pc1.create_offer().expect("create offer");
    pc1.set_local_description(&offer).expect("pc1 local");
    pc2.set_remote_description(&offer).expect("pc2 remote");
    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer).expect("pc2 local");
    pc1.set_remote_description(&answer).expect("pc1 remote");

    (pc1, pc2, s1, s2)
}

/// Replay pc1's gathered candidates onto pc2 the way a trickle client would
/// (gathering on loopback is prompt but asynchronous, so the drain polls).
fn forward_gathered(from: &Shared, to: &PeerConnection) -> usize {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut added = 0usize;
    while Instant::now() < deadline {
        while let Some(c) = from.ice.lock().unwrap().pop_front() {
            if !c.candidate.is_empty() {
                to.add_ice_candidate(&c).expect("a real candidate applies");
                added += 1;
            }
        }
        if added > 0 {
            return added;
        }
        thread::sleep(Duration::from_millis(20));
    }
    added
}

#[test]
fn the_end_of_candidates_marker_is_accepted() {
    let (_pc1, pc2, s1, _s2) = negotiate();

    // The baseline the marker must not disturb: real trickled candidates.
    assert!(
        forward_gathered(&s1, &pc2) > 0,
        "ICE gathering produced nothing to trickle"
    );

    pc2.add_ice_candidate(&IceCandidate {
        candidate: String::new(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
    })
    .expect("the marker is accepted with an addressing m-line");
    pc2.add_ice_candidate(&IceCandidate {
        candidate: String::new(),
        sdp_mid: None,
        sdp_mline_index: None,
    })
    .expect("a bare marker is not rejected over its missing address");
}

#[test]
fn a_garbage_candidate_still_fails_the_parse() {
    let (_pc1, pc2, s1, _s2) = negotiate();

    assert!(
        forward_gathered(&s1, &pc2) > 0,
        "ICE gathering produced nothing to trickle"
    );
    assert!(
        pc2.add_ice_candidate(&IceCandidate {
            candidate: "bogus".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        })
        .is_err(),
        "an unparseable candidate must not ride the marker's exemption"
    );
}
