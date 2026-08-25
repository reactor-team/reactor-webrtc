//! AdmMode: a factory can be created with either the synthetic or the platform
//! audio device module. Smoke-tests both create a working factory + peer
//! connection (no capture starts, so no mic is opened).
#![cfg(have_libwebrtc)]

use reactor_webrtc::{AdmMode, PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration};

fn smoke(mode: AdmMode) {
    let factory = PeerConnectionFactory::builder()
        .with_adm(mode)
        .build()
        .expect("factory");
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("peer connection");
    let _dc = pc.create_data_channel("probe").expect("data channel");
    println!("AdmMode::{mode:?} → factory + peer connection OK");
}

#[test]
fn synthetic_adm() {
    smoke(AdmMode::Synthetic);
}

// The platform ADM opens the OS audio stack; WebRTC's adm_helpers.cc RTC_CHECKs
// that adm->Init() succeeds and *aborts* (SIGABRT) if it can't — which it can't
// on a headless host (no audio device). That abort is uncatchable from Rust, so
// this is #[ignore]d by default; run `cargo test -- --ignored` on a machine with
// audio. The synthetic path (above) already covers the factory/pc/dc wiring.
#[test]
#[ignore = "platform ADM aborts without an OS audio device; run with --ignored where audio exists"]
fn platform_adm() {
    smoke(AdmMode::Platform);
}
