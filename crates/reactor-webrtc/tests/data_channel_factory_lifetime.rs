//! Does a `DataChannel` outlive the `PeerConnectionFactory` that (indirectly,
//! through its `PeerConnection`) produced it?
//!
//! `DataChannel` has the same shape `PeerConnection` had before it held a
//! factory handle: a raw pointer and an observer box, nothing keeping the
//! factory's signaling/network threads alive. A `PeerConnection` now carries
//! its own factory handle, so dropping the factory alone no longer crashes
//! while a connection is still alive — but a `DataChannel` can be detached and
//! outlive *both* the connection and the factory that ultimately owns its
//! threads, and nothing stops that today.
//!
//! Settled the same way as `factory_lifetime.rs`: create a factory, a peer
//! connection, and a data channel from it, drop the connection and the
//! factory, then touch (and drop) the data channel. If nothing keeps the
//! factory's threads alive on the data channel's behalf, this does not fail
//! cleanly — it crashes the process.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test -p reactor-webrtc --test data_channel_factory_lifetime
//! ```
#![cfg(have_libwebrtc)]

use reactor_webrtc::{PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration};

#[test]
fn data_channel_survives_factory_and_connection_drop() {
    let factory = PeerConnectionFactory::new().expect("factory creation failed");
    let pc = factory
        .create_peer_connection(&RtcConfiguration::default(), PeerConnectionObserver::new())
        .expect("peer connection creation failed");
    let dc = pc
        .create_data_channel("probe")
        .expect("data channel creation failed");

    assert_eq!(dc.label(), "probe");

    // Release the connection and the factory first, leaving the data channel
    // as the only thing that might still need the factory's threads alive.
    drop(pc);
    drop(factory);

    // Drops the native data channel, which dispatches onto the network thread
    // the (allegedly gone) factory used to own.
    drop(dc);
}
