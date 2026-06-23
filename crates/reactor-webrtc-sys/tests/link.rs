//! Real-link proof: this test only compiles when a native libwebrtc is
//! resolved (build.rs emits `cfg(have_libwebrtc)` then). It drives real WebRTC
//! objects through the C++ glue: codec factories, a PeerConnectionFactory, and
//! a PeerConnection with the callback bridge (offer + data channel + observer).
//!
//! Run it against a locally built lib:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc-sys -- --nocapture
//! ```
//!
//! Without the env var, `cargo test` builds an empty test binary (the symbols
//! have nothing to link against), so the workspace still checks cleanly.
#![cfg(have_libwebrtc)]

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc_sys::{
    reactor_webrtc_abi_version, reactor_webrtc_data_channel_destroy, reactor_webrtc_factory_create,
    reactor_webrtc_factory_destroy, reactor_webrtc_peer_connection_create,
    reactor_webrtc_peer_connection_create_data_channel,
    reactor_webrtc_peer_connection_create_offer, reactor_webrtc_peer_connection_destroy,
    reactor_webrtc_selftest, PeerConnectionCallbacks,
};

#[derive(Default)]
struct Stats {
    renegotiation: AtomicU32,
    ice_candidate: AtomicU32,
    ice_gathering: AtomicU32,
    signaling: AtomicU32,
}

extern "C" fn on_renegotiation(ud: *mut c_void) {
    unsafe { &*(ud as *const Stats) }
        .renegotiation
        .fetch_add(1, Ordering::SeqCst);
}
extern "C" fn on_ice_candidate(
    ud: *mut c_void,
    _mid: *const c_char,
    _idx: i32,
    _cand: *const c_char,
) {
    unsafe { &*(ud as *const Stats) }
        .ice_candidate
        .fetch_add(1, Ordering::SeqCst);
}
extern "C" fn on_ice_gathering(ud: *mut c_void, _state: i32) {
    unsafe { &*(ud as *const Stats) }
        .ice_gathering
        .fetch_add(1, Ordering::SeqCst);
}
extern "C" fn on_signaling(ud: *mut c_void, _state: i32) {
    unsafe { &*(ud as *const Stats) }
        .signaling
        .fetch_add(1, Ordering::SeqCst);
}

type OfferTx = SyncSender<Result<(String, String), String>>;

extern "C" fn on_offer_ok(ud: *mut c_void, ty: *const c_char, sdp: *const c_char) {
    let tx = unsafe { &*(ud as *const OfferTx) };
    let ty = unsafe { CStr::from_ptr(ty) }.to_string_lossy().into_owned();
    let sdp = unsafe { CStr::from_ptr(sdp) }
        .to_string_lossy()
        .into_owned();
    let _ = tx.try_send(Ok((ty, sdp)));
}
extern "C" fn on_offer_err(ud: *mut c_void, message: *const c_char) {
    let tx = unsafe { &*(ud as *const OfferTx) };
    let msg = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    let _ = tx.try_send(Err(msg));
}

fn wait_until(predicate: impl Fn() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[test]
fn links_and_runs_libwebrtc() {
    // SAFETY: every symbol is implemented by the C++ glue compiled in build.rs
    // and resolved against our libwebrtc.a.
    unsafe {
        assert_eq!(reactor_webrtc_abi_version(), 1, "ABI version mismatch");

        // 1. Codec factories.
        let mut buf = [0u8; 1024];
        let n = reactor_webrtc_selftest(buf.as_mut_ptr() as *mut c_char, buf.len() as i32);
        assert!(n > 0, "expected at least one codec from libwebrtc");
        let codecs = CStr::from_ptr(buf.as_ptr() as *const c_char).to_string_lossy();
        println!("libwebrtc linked OK — {n} codecs: {codecs}");
        let lower = codecs.to_lowercase();
        assert!(lower.contains("opus"), "expected Opus, got: {codecs}");
        assert!(lower.contains("vp8"), "expected VP8, got: {codecs}");

        // 2. PeerConnectionFactory.
        let factory = reactor_webrtc_factory_create();
        assert!(!factory.is_null(), "PeerConnectionFactory creation failed");
        println!("PeerConnectionFactory created OK");

        // 3. PeerConnection with the observer bridge.
        let stats = Box::new(Stats::default());
        let callbacks = PeerConnectionCallbacks {
            userdata: &*stats as *const Stats as *mut c_void,
            on_signaling_change: Some(on_signaling),
            on_connection_change: None,
            on_ice_gathering_change: Some(on_ice_gathering),
            on_ice_candidate: Some(on_ice_candidate),
            on_data_channel: None,
            on_renegotiation_needed: Some(on_renegotiation),
        };
        let config =
            CString::new(r#"{"iceServers":[{"urls":"stun:stun.l.google.com:19302"}]}"#).unwrap();
        let pc = reactor_webrtc_peer_connection_create(factory, config.as_ptr(), &callbacks);
        assert!(!pc.is_null(), "PeerConnection creation failed");
        println!("PeerConnection created OK");

        // 4. Data channel → must trigger the (async) renegotiation observer.
        let label = CString::new("reactor").unwrap();
        let dc = reactor_webrtc_peer_connection_create_data_channel(pc, label.as_ptr());
        assert!(!dc.is_null(), "data channel creation failed");
        assert!(
            wait_until(
                || stats.renegotiation.load(Ordering::SeqCst) > 0,
                Duration::from_secs(3)
            ),
            "expected OnRenegotiationNeeded after creating a data channel"
        );
        println!(
            "observer fired: renegotiation={}",
            stats.renegotiation.load(Ordering::SeqCst)
        );

        // 5. Create an offer (async CreateSessionDescriptionObserver bridge).
        let (tx, rx) = sync_channel::<Result<(String, String), String>>(1);
        let tx_ptr = Box::into_raw(Box::new(tx));
        reactor_webrtc_peer_connection_create_offer(
            pc,
            tx_ptr as *mut c_void,
            on_offer_ok,
            on_offer_err,
        );
        let offer = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("offer callback did not fire");
        drop(Box::from_raw(tx_ptr));
        let (ty, sdp) = offer.expect("CreateOffer failed");
        assert_eq!(ty, "offer");
        assert!(sdp.contains("v=0"), "offer SDP not well-formed:\n{sdp}");
        assert!(
            sdp.contains("m=application"),
            "offer should include the data channel m-line:\n{sdp}"
        );
        println!(
            "offer created OK ({} bytes, includes m=application)",
            sdp.len()
        );

        // 6. Teardown.
        reactor_webrtc_data_channel_destroy(dc);
        reactor_webrtc_peer_connection_destroy(pc);
        reactor_webrtc_factory_destroy(factory);
        drop(stats);
        println!("teardown OK");
    }
}
