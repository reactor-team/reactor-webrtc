//! Real-link proof: these tests only compile when a native libwebrtc is
//! resolved (build.rs emits `cfg(have_libwebrtc)` then). They drive real WebRTC
//! objects through the C++ glue: codec factories, a PeerConnectionFactory, and
//! PeerConnections with the callback bridge — up to a full local loopback that
//! negotiates, connects, and sends/receives real audio + video.
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
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reactor_webrtc_sys::{
    reactor_webrtc_abi_version, reactor_webrtc_audio_track_add_sink,
    reactor_webrtc_audio_track_create, reactor_webrtc_data_channel_destroy,
    reactor_webrtc_factory_create, reactor_webrtc_factory_create_with_adm,
    reactor_webrtc_factory_destroy, reactor_webrtc_factory_push_audio_frame,
    reactor_webrtc_factory_set_adm_playout_enabled, reactor_webrtc_media_stream_track_destroy,
    reactor_webrtc_peer_connection_add_ice_candidate, reactor_webrtc_peer_connection_add_track,
    reactor_webrtc_peer_connection_create, reactor_webrtc_peer_connection_create_answer,
    reactor_webrtc_peer_connection_create_data_channel,
    reactor_webrtc_peer_connection_create_offer, reactor_webrtc_peer_connection_destroy,
    reactor_webrtc_peer_connection_set_local_description,
    reactor_webrtc_peer_connection_set_remote_description, reactor_webrtc_selftest,
    reactor_webrtc_video_track_add_sink, reactor_webrtc_video_track_create,
    reactor_webrtc_video_track_push_frame, MediaStreamTrack, PeerConnection,
    PeerConnectionCallbacks, PeerConnectionFactory, ReactorRtcConfig,
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
    video_frames: AtomicU32,
    audio_frames: AtomicU32,
    // Received remote track handles (*mut MediaStreamTrack as usize).
    recv_tracks: Mutex<Vec<usize>>,
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
extern "C" fn ctx_on_video(ud: *mut c_void, _bgra: *const u8, _width: c_int, _height: c_int) {
    let ctx = unsafe { &*(ud as *const PcCtx) };
    ctx.video_frames.fetch_add(1, Ordering::SeqCst);
}
extern "C" fn ctx_on_audio(
    ud: *mut c_void,
    _pcm: *const i16,
    _rate: c_int,
    _channels: c_int,
    _frames: c_int,
) {
    let ctx = unsafe { &*(ud as *const PcCtx) };
    ctx.audio_frames.fetch_add(1, Ordering::SeqCst);
}
extern "C" fn ctx_on_track(ud: *mut c_void, track: *mut MediaStreamTrack) {
    let ctx = unsafe { &*(ud as *const PcCtx) };
    // Only the kind-matching call attaches a sink; the other is a no-op.
    unsafe {
        reactor_webrtc_video_track_add_sink(track, ud, ctx_on_video);
        reactor_webrtc_audio_track_add_sink(track, ud, ctx_on_audio);
    }
    ctx.recv_tracks.lock().unwrap().push(track as usize);
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
        on_track: Some(ctx_on_track),
    }
}

/// Create a PeerConnection, returning the glue's failure reason on error.
unsafe fn create_pc(
    factory: *mut PeerConnectionFactory,
    config: *const ReactorRtcConfig,
    callbacks: *const PeerConnectionCallbacks,
) -> Result<*mut PeerConnection, String> {
    let mut err = [0 as c_char; 256];
    let pc = reactor_webrtc_peer_connection_create(
        factory,
        config,
        callbacks,
        err.as_mut_ptr(),
        err.len() as c_int,
    );
    if pc.is_null() {
        return Err(CStr::from_ptr(err.as_ptr()).to_string_lossy().into_owned());
    }
    Ok(pc)
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
        assert_eq!(reactor_webrtc_abi_version(), 2, "ABI version mismatch");

        // Codec factories.
        let mut buf = [0u8; 1024];
        let n = reactor_webrtc_selftest(buf.as_mut_ptr() as *mut c_char, buf.len() as i32);
        assert!(n > 0, "expected at least one codec from libwebrtc");
        let codecs = CStr::from_ptr(buf.as_ptr() as *const c_char).to_string_lossy();
        println!("libwebrtc linked OK — {n} codecs: {codecs}");
        let lower = codecs.to_lowercase();
        assert!(lower.contains("opus"), "expected Opus, got: {codecs}");
        assert!(lower.contains("vp8"), "expected VP8, got: {codecs}");

        // PeerConnectionFactory + a PeerConnection we just create/destroy.
        let factory = reactor_webrtc_factory_create();
        assert!(!factory.is_null(), "PeerConnectionFactory creation failed");
        let pc = create_pc(factory, ptr::null(), ptr::null()).expect("PeerConnection creation");
        reactor_webrtc_peer_connection_destroy(pc);
        reactor_webrtc_factory_destroy(factory);
        println!("factory + peer connection lifecycle OK");
    }
}

#[test]
#[ignore = "platform ADM aborts (RTC_CHECK) without an OS audio device; run with --ignored where audio exists"]
fn platform_adm_factory_lifecycle() {
    // The platform-ADM path lets the media engine create the real device ADM
    // (CoreAudio/ALSA). WebRTC's adm_helpers.cc RTC_CHECKs adm->Init() and
    // aborts (uncatchable SIGABRT) when there's no audio device — as on headless
    // CI — so this is #[ignore]d there. Construct/destroy only; no capture.
    unsafe {
        let factory = reactor_webrtc_factory_create_with_adm(1);
        assert!(!factory.is_null(), "platform-ADM factory creation failed");
        // set_adm_playout_enabled is a no-op for the platform ADM but must not crash.
        reactor_webrtc_factory_set_adm_playout_enabled(factory, 0);
        let pc = create_pc(factory, ptr::null(), ptr::null())
            .expect("peer connection creation (platform ADM)");
        reactor_webrtc_peer_connection_destroy(pc);
        reactor_webrtc_factory_destroy(factory);
        println!("platform-ADM factory lifecycle OK");
    }
}

#[test]
fn loopback_two_peers_exchange_media() {
    // SAFETY: as above; drives a full offer/answer + ICE exchange between two
    // in-process PeerConnections, then streams audio + video one way.
    unsafe {
        let factory = reactor_webrtc_factory_create();
        assert!(!factory.is_null(), "factory creation failed");

        let ctx1 = Box::new(PcCtx::default());
        let ctx2 = Box::new(PcCtx::default());
        let cb1 = observer_for(&ctx1);
        let cb2 = observer_for(&ctx2);

        let pc1 = create_pc(factory, ptr::null(), &cb1).expect("pc1 creation");
        let pc2 = create_pc(factory, ptr::null(), &cb2).expect("pc2 creation");

        // pc1 gets a data channel + a video track + an audio track to negotiate.
        let dc_label = CString::new("reactor").unwrap();
        let dc = reactor_webrtc_peer_connection_create_data_channel(pc1, dc_label.as_ptr());
        assert!(!dc.is_null(), "data channel creation failed");

        let vid_id = CString::new("reactor-video").unwrap();
        let vtrack = reactor_webrtc_video_track_create(factory, vid_id.as_ptr());
        assert!(!vtrack.is_null(), "video track creation failed");
        assert_eq!(
            reactor_webrtc_peer_connection_add_track(pc1, vtrack),
            1,
            "add video failed"
        );

        let aud_id = CString::new("reactor-audio").unwrap();
        let atrack = reactor_webrtc_audio_track_create(factory, aud_id.as_ptr());
        assert!(!atrack.is_null(), "audio track creation failed");
        assert_eq!(
            reactor_webrtc_peer_connection_add_track(pc1, atrack),
            1,
            "add audio failed"
        );

        // offer: pc1 → (local on pc1, remote on pc2)
        let (oty, osdp) =
            run_sdp(|ud| reactor_webrtc_peer_connection_create_offer(pc1, ud, sdp_ok, sdp_err))
                .expect("create offer");
        assert!(
            osdp.contains("m=video"),
            "offer should include a video m-line"
        );
        assert!(
            osdp.contains("m=audio"),
            "offer should include an audio m-line"
        );
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
        println!("SDP offer/answer exchange complete (audio + video negotiated)");

        // Push BGRA video + int16 PCM audio into pc1 on a background thread.
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let stop = stop.clone();
            let vtrack = vtrack as usize;
            let factory = factory as usize;
            thread::spawn(move || {
                let vtrack = vtrack as *mut MediaStreamTrack;
                let factory = factory as *mut PeerConnectionFactory;
                let (w, h) = (320i32, 240i32);
                let mut bgra = vec![0u8; (w * h * 4) as usize];
                // 10ms of 48kHz stereo PCM.
                let (rate, channels, spc) = (48000i32, 2i32, 480i32);
                let mut pcm = vec![0i16; (spc * channels) as usize];
                let mut tick: u8 = 0;
                let mut i = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    // Vary content slightly each iteration.
                    bgra.iter_mut().for_each(|b| *b = tick);
                    tick = tick.wrapping_add(7);
                    for (j, s) in pcm.iter_mut().enumerate() {
                        *s = (((i as usize + j) % 256) as i16 - 128) * 64;
                    }
                    i = i.wrapping_add(1);
                    reactor_webrtc_factory_push_audio_frame(
                        factory,
                        pcm.as_ptr(),
                        spc,
                        rate,
                        channels,
                    );
                    if i % 3 == 0 {
                        reactor_webrtc_video_track_push_frame(vtrack, bgra.as_ptr(), w, h);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            })
        };

        // Trickle ICE both ways; wait for connect + received audio & video.
        let (mut fwd1, mut fwd2) = (0usize, 0usize);
        let start = Instant::now();
        loop {
            fwd1 += forward_candidates(&ctx1, pc2);
            fwd2 += forward_candidates(&ctx2, pc1);
            let connected =
                ctx1.connected.load(Ordering::SeqCst) && ctx2.connected.load(Ordering::SeqCst);
            let media = ctx2.video_frames.load(Ordering::SeqCst) > 0
                && ctx2.audio_frames.load(Ordering::SeqCst) > 0;
            if (connected && media) || start.elapsed() > Duration::from_secs(20) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
        pusher.join().unwrap();

        let (v, a) = (
            ctx2.video_frames.load(Ordering::SeqCst),
            ctx2.audio_frames.load(Ordering::SeqCst),
        );
        assert!(
            ctx1.connected.load(Ordering::SeqCst) && ctx2.connected.load(Ordering::SeqCst),
            "loopback did not connect (candidates forwarded {fwd1}+{fwd2})",
        );
        assert!(v > 0, "pc2 received no video frames");
        assert!(a > 0, "pc2 received no audio frames");
        println!("loopback connected ✅ — pc2 received {v} video + {a} audio frame(s)");

        // Teardown: received tracks first, then local tracks, channel, pcs.
        for ctx in [&ctx1, &ctx2] {
            for p in ctx.recv_tracks.lock().unwrap().drain(..) {
                reactor_webrtc_media_stream_track_destroy(p as *mut MediaStreamTrack);
            }
        }
        reactor_webrtc_media_stream_track_destroy(vtrack);
        reactor_webrtc_media_stream_track_destroy(atrack);
        reactor_webrtc_data_channel_destroy(dc);
        reactor_webrtc_peer_connection_destroy(pc1);
        reactor_webrtc_peer_connection_destroy(pc2);
        reactor_webrtc_factory_destroy(factory);
        drop((ctx1, ctx2));
        println!("teardown OK");
    }
}
