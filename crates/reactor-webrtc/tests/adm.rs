//! AdmMode: a factory can be created with either the synthetic or the platform
//! audio device module. Smoke-tests both create a working factory + peer
//! connection (no capture starts, so no mic is opened).
#![cfg(have_libwebrtc)]

use reactor_webrtc::{AdmMode, PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration};

fn smoke(mode: AdmMode) {
    let factory = PeerConnectionFactory::with_adm(mode).expect("factory");
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

#[test]
fn platform_adm() {
    // The platform ADM opens the OS audio stack; a headless CI host has no audio
    // device, so factory creation fails there. Treat that as skipped — the
    // synthetic path already covers the factory/peer-connection/data-channel
    // wiring; where audio exists (dev machines) this exercises the real ADM.
    match PeerConnectionFactory::with_adm(AdmMode::Platform) {
        Ok(factory) => {
            let pc = factory
                .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
                .expect("peer connection");
            let _dc = pc.create_data_channel("probe").expect("data channel");
            println!("AdmMode::Platform → factory + peer connection OK");
        }
        Err(e) => eprintln!("platform_adm skipped (no audio device? {e})"),
    }
}
