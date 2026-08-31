//! Can an application choose its own ICE credentials?
//!
//! An edge relay that routes by ICE ufrag needs the ufrag to be a value *it*
//! issued, not one libwebrtc generated. libwebrtc exposes no setter for this, so
//! the question is whether the credentials in the local session description are
//! what actually reach the ICE transport.
//!
//! Reading the source says yes — `JsepTransport::SetLocalJsepTransportDescription`
//! takes `IceParameters` straight from the description's transport description,
//! and the only guard on a local description is `VerifyIceUfragPwdPresent`, which
//! checks presence rather than provenance. This test settles it by observation
//! instead of by reading.
//!
//! The proof is a loopback with one side's credentials replaced. If the transport
//! ignored them, the peer would sign its connectivity checks with the substituted
//! ufrag while the transport still expected the generated one, every check would
//! fail its integrity test, and ICE would never connect. Connecting therefore
//! means the substitution took effect.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test -p reactor-webrtc --test ice_credentials
//! ```
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc::{
    IceCandidate, PeerConnection, PeerConnectionFactory, PeerConnectionObserver,
    PeerConnectionState, RtcConfiguration,
};

/// Shaped like what an edge relay would mint: 30 `ice-char` characters, well over
/// the four libwebrtc generates and inside RFC 8445's 4..=256.
const RELAY_UFRAG: &str = "CgAHFcpsamqt/IIl8YtGLBP8al/dIA";
/// 32 characters, over RFC 8445's 22-character minimum.
const RELAY_PWD: &str = "iMyV3ZlbyUC8SBiy/AeG2OVaSJ5di54s";

#[derive(Default)]
struct Shared {
    ice: Mutex<VecDeque<IceCandidate>>,
    connected: AtomicBool,
}

fn make_peer(
    factory: &PeerConnectionFactory,
    config: &RtcConfiguration,
) -> (PeerConnection, Arc<Shared>) {
    let shared = Arc::new(Shared::default());
    let observer = PeerConnectionObserver::new()
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
        });
    let pc = factory
        .create_peer_connection(config, observer)
        .expect("create peer connection");
    (pc, shared)
}

fn forward_ice(from: &Shared, to: &PeerConnection) {
    while let Some(c) = {
        let mut q = from.ice.lock().unwrap();
        q.pop_front()
    } {
        let _ = to.add_ice_candidate(&c);
    }
}

#[test]
fn generated_credentials_are_not_ours() {
    // Establishes the baseline the substitution has to overcome: libwebrtc picks
    // its own value. Its *length* is reported rather than asserted — today it is
    // four characters, far too small to carry a routing token, but that is an
    // upstream detail and this repo bumps the prebuilt regularly. Failing here
    // because libwebrtc improved would be noise.
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let (pc, _) = make_peer(&factory, &RtcConfiguration::default());
    pc.create_data_channel("probe").expect("data channel");
    let offer = pc.create_offer().expect("create offer");

    let found = offer.ice_ufrags();
    assert!(!found.is_empty(), "an offer must carry an ice-ufrag");
    for u in &found {
        assert_ne!(*u, RELAY_UFRAG);
    }
    println!(
        "libwebrtc generated ufrag(s): {found:?} ({} chars each)",
        found[0].len()
    );
}

#[test]
fn a_substituted_local_description_is_accepted() {
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let (pc, _) = make_peer(&factory, &RtcConfiguration::default());
    pc.create_data_channel("probe").expect("data channel");

    let offer = pc.create_offer().expect("create offer");
    let munged = offer
        .with_ice_credentials(RELAY_UFRAG, RELAY_PWD)
        .expect("substitution rejected");
    assert!(munged.ice_ufrags().iter().all(|u| *u == RELAY_UFRAG));

    pc.set_local_description(&munged)
        .expect("libwebrtc rejected a local description with substituted ICE credentials");
    println!("substituted local description accepted");
}

#[test]
fn a_substituted_ufrag_actually_reaches_the_ice_transport() {
    // The real question. The offerer's credentials are replaced before its local
    // description is set; the answerer therefore signs every connectivity check
    // with the substituted ufrag and password. If the offerer's transport had kept
    // the generated pair, no check would ever authenticate and ICE would stall.
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);

    pc1.create_data_channel("relay-probe")
        .expect("data channel");

    let offer = pc1.create_offer().expect("create offer");
    let generated: Vec<String> = offer.ice_ufrags().iter().map(|s| s.to_string()).collect();
    let offer = offer
        .with_ice_credentials(RELAY_UFRAG, RELAY_PWD)
        .expect("substitution rejected");

    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");
    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    let start = Instant::now();
    loop {
        forward_ice(&s1, &pc2);
        forward_ice(&s2, &pc1);
        if (s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst))
            || start.elapsed() > Duration::from_secs(20)
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst),
        "ICE did not connect with substituted credentials (generated were {generated:?}); \
         the transport kept its own ufrag rather than the one in the local description"
    );
    println!(
        "connected with substituted ufrag {RELAY_UFRAG:?} in {:?} \
         (libwebrtc had generated {generated:?})",
        start.elapsed()
    );
}

#[test]
fn a_renegotiation_description_has_no_candidate_level_ufrag() {
    // Guards an upstream assumption `with_ice_credentials` relies on.
    //
    // A first offer carries no candidates, so substituting media-level credentials
    // is unambiguous. A renegotiation-time offer *does* carry the candidates
    // gathered since — and RFC 8839 lets a candidate line end with an optional
    // `ufrag <value>` token. If libwebrtc emitted it, substituting only the
    // media-level attributes would leave the two disagreeing, and a remote peer
    // could read those candidates as belonging to a previous ICE generation and
    // discard them.
    //
    // It does not emit it. This test exists so that a prebuilt bump which changes
    // that is caught here rather than in the field.
    let factory = PeerConnectionFactory::builder().build().expect("factory");
    let config = RtcConfiguration::default();
    let (pc1, s1) = make_peer(&factory, &config);
    let (pc2, s2) = make_peer(&factory, &config);
    pc1.create_data_channel("reneg").expect("data channel");

    let offer = pc1
        .create_offer()
        .expect("create offer")
        .with_ice_credentials(RELAY_UFRAG, RELAY_PWD)
        .expect("substitution rejected");
    assert_eq!(
        offer
            .sdp
            .lines()
            .filter(|l| l.starts_with("a=candidate:"))
            .count(),
        0,
        "a first offer is expected to predate gathering"
    );

    pc1.set_local_description(&offer).expect("pc1 local offer");
    pc2.set_remote_description(&offer)
        .expect("pc2 remote offer");
    let answer = pc2.create_answer().expect("create answer");
    pc2.set_local_description(&answer)
        .expect("pc2 local answer");
    pc1.set_remote_description(&answer)
        .expect("pc1 remote answer");

    let start = Instant::now();
    loop {
        forward_ice(&s1, &pc2);
        forward_ice(&s2, &pc1);
        if (s1.connected.load(Ordering::SeqCst) && s2.connected.load(Ordering::SeqCst))
            || start.elapsed() > Duration::from_secs(20)
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(s1.connected.load(Ordering::SeqCst), "did not connect");

    // Now gathering has run, so this one carries candidates.
    let reneg = pc1.create_offer().expect("renegotiation offer");
    let candidates: Vec<&str> = reneg
        .sdp
        .lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .collect();
    assert!(
        !candidates.is_empty(),
        "expected a renegotiation offer to carry gathered candidates"
    );
    for c in &candidates {
        assert!(
            !c.contains(" ufrag "),
            "libwebrtc now emits a candidate-level ufrag; \
             with_ice_credentials must rewrite it too:\n  {c}"
        );
    }
    // And the media-level value is still the substituted one.
    assert!(reneg.ice_ufrags().iter().all(|u| *u == RELAY_UFRAG));
    println!(
        "renegotiation offer: {} candidate line(s), none carrying a ufrag token",
        candidates.len()
    );
}
