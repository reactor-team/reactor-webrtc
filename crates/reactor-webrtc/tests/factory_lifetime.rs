//! Does a `PeerConnection` outlive the `PeerConnectionFactory` that created it?
//!
//! `PeerConnectionFactory` owns libwebrtc's signaling/worker/network threads;
//! `PeerConnection` only borrows a raw pointer into the native peer connection
//! those threads run. Nothing in `PeerConnection` keeps the factory alive, so
//! if a caller drops the factory before the connections it created, the
//! factory's `Drop` tears its threads down immediately and every live
//! `PeerConnection` is left holding a pointer into what those threads used to
//! back — the shape of a use-after-free a segfault (not a clean panic) should
//! catch.
//!
//! This is exactly the invariant `reactor-webrtc-py`'s test suite works around
//! by construction (a single session-scoped factory, never recreated — see
//! `crates/reactor-webrtc-py/tests/conftest.py`) rather than one this crate
//! enforces itself. This test settles it by observation: create a factory,
//! create a peer connection from it, drop the factory first, then touch the
//! peer connection. If the factory's destruction is not held back by every
//! object it produced, this test does not fail cleanly — it crashes the
//! process, the same way an ordinary Python script that lets normal garbage
//! collection tear these down in the wrong order does.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test -p reactor-webrtc --test factory_lifetime
//! ```
#![cfg(have_libwebrtc)]

use reactor_webrtc::{PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration};

#[test]
fn peer_connection_survives_factory_drop() {
    let factory = PeerConnectionFactory::builder()
        .build()
        .expect("factory creation failed");
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("peer connection creation failed");

    drop(factory);

    // Touches the native peer connection, which dispatches onto the
    // signaling thread the (allegedly gone) factory used to own.
    let offer = pc.create_offer();
    assert!(
        offer.is_ok(),
        "create_offer failed after factory drop: {offer:?}"
    );
}
