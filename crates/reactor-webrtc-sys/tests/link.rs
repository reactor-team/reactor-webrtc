//! Real-link proof: these tests only compile when a native libwebrtc is
//! resolved (build.rs emits `cfg(have_libwebrtc)` then). They drive real WebRTC
//! objects through the C++ glue: codec factories, a PeerConnectionFactory, and
//! PeerConnections with the callback bridge — up to a full local loopback
//! (offer/answer + ICE trickle → connected).
//!
//! Run against a locally built lib:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test -p reactor-webrtc-sys -- --nocapture
//! ```
//!
//! Without the env var, `cargo test` builds an empty test binary (the symbols
//! have nothing to link against), so the workspace still checks cleanly.
#![cfg(have_libwebrtc)]

use std::collections::VecDeque;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc_sys::{
    reactor_webrtc_abi_version, reactor_webrtc_data_channel_destroy, reactor_webrtc_factory_create,
    reactor_webrtc_factory_destroy, reactor_webrtc_peer_connection_add_ice_candidate,
    reactor_webrtc_peer_connection_create, reactor_webrtc_peer_connection_create_answer,
    reactor_webrtc_peer_connection_create_data_channel,
    reactor_webrtc_peer_connection_create_offer, reactor_webrtc_peer_connection_destroy,
    reactor_webrtc_peer_connection_set_local_description,
    reactor_webrtc_peer_connection_set_remote_description, reactor_webrtc_selftest, PeerConnection,
    PeerConnectionCallbacks,
};

// PeerConnectionState::kConnected (enum order in peer_connection_interface.h).
const PEER_CONNECTION_STATE_CONNECTED: i32 = 2;

// ── async-op bridges: block the test thread on a one-shot callback ───────────

type SdpTx = SyncSender<Result<(String, String), String>>;
type CompleteTx = SyncSender<Result<(), String>>;

extern "C" fn sdp_ok(ud: *mut c_void, ty: *const c_char, sdp: *const c_char) {
    let tx = unsafe { &*(ud as *const SdpTx) };
    let ty = unsafe { CStr::from_ptr(ty) }.to_string_lossy().into_owned();
    let sdp = unsafe { CStr::from_ptr(sdp) }
        .to_string_lossy()
        .into_owned();
    let _ = tx.try_send(Ok((ty, sdp)));
}
extern "C" fn sdp_err(ud: *mut c_void, message: *const c_char) {
    let tx = unsafe { &*(ud as *const SdpTx) };
    let msg = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    let _ = tx.try_send(Err(msg));
}
extern "C" fn complete_cb(ud: *mut c_void, error: *const c_char) {
    let tx = unsafe { &*(ud as *const CompleteTx) };
    let r = if error.is_null() {
        Ok(())
    } else {
        Err(unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned())
    };
    let _ = tx.try_send(r);
}
extern "C" fn noop_complete(_ud: *mut c_void, _error: *const c_char) {}

/// Invoke an async create-offer/answer and block for its result.
unsafe fn run_sdp(call: impl FnOnce(*mut c_void)) -> Result<(String, String), String> {
    let (tx, rx) = sync_channel::<Result<(String, String), String>>(1);
    let p = Box::into_raw(Box::new(tx));
    call(p as *mut c_void);
    let r = rx.recv_timeout(Duration::from_secs(5));
    drop(Box::from_raw(p));
    r.expect("sdp callback did not fire")
}

/// Invoke an async set-description and block for completion.
unsafe fn run_complete(call: impl FnOnce(*mut c_void)) -> Result<(), String> {
    let (tx, rx) = sync_channel::<Result<(), String>>(1);
    let p = Box::into_raw(Box::new(tx));
    call(p as *mut c_void);
    let r = rx.recv_timeout(Duration::from_secs(5));
    drop(Box::from_raw(p));
    r.expect("completion callback did not fire")
}

// ── PeerConnection observer context for the loopback ─────────────────────────

#[derive(Default)]
struct PcCtx {
    ice: Mutex<VecDeque<(String, i32, String)>>,
    connected: AtomicBool,
}

extern "C" fn ctx_on_ice(ud: *mut c_void, mid: *const c_char, idx: i32, cand: *const c_char) {
    let ctx = unsafe { &*(ud as *const PcCtx) };
    let s = |p: *const c_char| {
        if p.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    };
    ctx.ice.lock().unwrap().push_back((s(mid), idx, s(cand)));
}
extern "C" fn ctx_on_conn(ud: *mut c_void, state: i32) {
    let ctx = unsafe { &*(ud as *const PcCtx) };
    if state == PEER_CONNECTION_STATE_CONNECTED {
        ctx.connected.store(true, Ordering::SeqCst);
    }
}

fn observer_for(ctx: &PcCtx) -> PeerConnectionCallbacks {
    PeerConnectionCallbacks {
        userdata: ctx as *const PcCtx as *mut c_void,
        on_signaling_change: None,
        on_connection_change: Some(ctx_on_conn),
        on_ice_gathering_change: None,
        on_ice_candidate: Some(ctx_on_ice),
        on_data_channel: None,
        on_renegotiation_needed: None,
    }
}

/// Drain `src`'s gathered candidates into `dst`. Returns how many were added.
fn forward_candidates(src: &PcCtx, dst: *mut PeerConnection) -> usize {
    let mut n = 0;
    loop {
        let item = src.ice.lock().unwrap().pop_front();
        let Some((mid, idx, cand)) = item else { break };
        let mid_c = CString::new(mid).unwrap();
        let cand_c = CString::new(cand).unwrap();
        unsafe {
            reactor_webrtc_peer_connection_add_ice_candidate(
                dst,
                mid_c.as_ptr(),
                idx,
                cand_c.as_ptr(),
                ptr::null_mut(),
                noop_complete,
            );
        }
        n += 1;
    }
    n
}

#[test]
fn links_and_runs_libwebrtc() {
    // SAFETY: every symbol is implemented by the C++ glue compiled in build.rs
    // and resolved against our libwebrtc.a.
    unsafe {
        assert_eq!(reactor_webrtc_abi_version(), 1, "ABI version mismatch");

        // Codec factories.
        let mut buf = [0u8; 1024];
        let n = reactor_webrtc_selftest(buf.as_mut_ptr() as *mut c_char, buf.len() as i32);
        assert!(n > 0, "expected at least one codec from libwebrtc");
        let codecs = CStr::from_ptr(buf.as_ptr() as *const c_char).to_string_lossy();
        println!("libwebrtc linked OK — {n} codecs: {codecs}");
        let lower = codecs.to_lowercase();
        assert!(lower.contains("opus"), "expected Opus, got: {codecs}");
        assert!(lower.contains("vp8"), "expected VP8, got: {codecs}");

        // PeerConnectionFactory + a PeerConnection that we just create/destroy.
        let factory = reactor_webrtc_factory_create();
        assert!(!factory.is_null(), "PeerConnectionFactory creation failed");
        let pc = reactor_webrtc_peer_connection_create(factory, ptr::null(), ptr::null());
        assert!(!pc.is_null(), "PeerConnection creation failed");
        reactor_webrtc_peer_connection_destroy(pc);
        reactor_webrtc_factory_destroy(factory);
        println!("factory + peer connection lifecycle OK");
    }
}

#[test]
fn loopback_two_peers_connect() {
    // SAFETY: as above; this test drives a full offer/answer + ICE exchange
    // between two PeerConnections in-process and waits for them to connect.
    unsafe {
        let factory = reactor_webrtc_factory_create();
        assert!(!factory.is_null(), "factory creation failed");

        let ctx1 = Box::new(PcCtx::default());
        let ctx2 = Box::new(PcCtx::default());
        let cb1 = observer_for(&ctx1);
        let cb2 = observer_for(&ctx2);

        let pc1 = reactor_webrtc_peer_connection_create(factory, ptr::null(), &cb1);
        let pc2 = reactor_webrtc_peer_connection_create(factory, ptr::null(), &cb2);
        assert!(
            !pc1.is_null() && !pc2.is_null(),
            "peer connection creation failed"
        );

        // A data channel gives the offer something to negotiate (SCTP/DTLS).
        let label = CString::new("reactor").unwrap();
        let dc = reactor_webrtc_peer_connection_create_data_channel(pc1, label.as_ptr());
        assert!(!dc.is_null(), "data channel creation failed");

        // offer: pc1 → (local on pc1, remote on pc2)
        let (oty, osdp) =
            run_sdp(|ud| reactor_webrtc_peer_connection_create_offer(pc1, ud, sdp_ok, sdp_err))
                .expect("create offer");
        let (oty, osdp) = (CString::new(oty).unwrap(), CString::new(osdp).unwrap());
        run_complete(|ud| {
            reactor_webrtc_peer_connection_set_local_description(
                pc1,
                oty.as_ptr(),
                osdp.as_ptr(),
                ud,
                complete_cb,
            )
        })
        .expect("pc1 set local offer");
        run_complete(|ud| {
            reactor_webrtc_peer_connection_set_remote_description(
                pc2,
                oty.as_ptr(),
                osdp.as_ptr(),
                ud,
                complete_cb,
            )
        })
        .expect("pc2 set remote offer");

        // answer: pc2 → (local on pc2, remote on pc1)
        let (aty, asdp) =
            run_sdp(|ud| reactor_webrtc_peer_connection_create_answer(pc2, ud, sdp_ok, sdp_err))
                .expect("create answer");
        let (aty, asdp) = (CString::new(aty).unwrap(), CString::new(asdp).unwrap());
        run_complete(|ud| {
            reactor_webrtc_peer_connection_set_local_description(
                pc2,
                aty.as_ptr(),
                asdp.as_ptr(),
                ud,
                complete_cb,
            )
        })
        .expect("pc2 set local answer");
        run_complete(|ud| {
            reactor_webrtc_peer_connection_set_remote_description(
                pc1,
                aty.as_ptr(),
                asdp.as_ptr(),
                ud,
                complete_cb,
            )
        })
        .expect("pc1 set remote answer");
        println!("SDP offer/answer exchange complete");

        // Trickle ICE both ways and wait for both sides to connect.
        let (mut fwd1, mut fwd2) = (0usize, 0usize);
        let start = Instant::now();
        loop {
            fwd1 += forward_candidates(&ctx1, pc2);
            fwd2 += forward_candidates(&ctx2, pc1);
            let connected =
                ctx1.connected.load(Ordering::SeqCst) && ctx2.connected.load(Ordering::SeqCst);
            if connected || start.elapsed() > Duration::from_secs(15) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        assert!(
            ctx1.connected.load(Ordering::SeqCst) && ctx2.connected.load(Ordering::SeqCst),
            "loopback did not connect (pc1={}, pc2={}, candidates forwarded {fwd1}+{fwd2})",
            ctx1.connected.load(Ordering::SeqCst),
            ctx2.connected.load(Ordering::SeqCst),
        );
        println!("loopback connected ✅ (forwarded {fwd1}+{fwd2} ICE candidates)");

        reactor_webrtc_data_channel_destroy(dc);
        reactor_webrtc_peer_connection_destroy(pc1);
        reactor_webrtc_peer_connection_destroy(pc2);
        reactor_webrtc_factory_destroy(factory);
        drop((ctx1, ctx2));
        println!("teardown OK");
    }
}
