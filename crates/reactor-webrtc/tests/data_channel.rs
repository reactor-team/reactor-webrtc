//! Integration tests for the DataChannel API.
//!
//! Two local PeerConnections exchange messages over a data channel negotiated
//! by SDP.  Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=webrtc-build/out/mac-arm64-release/dist \
//!   cargo test --test data_channel -- --nocapture
//! ```
//!
//! Callbacks on the signaling thread, libwebrtc's default; the same suite runs
//! with them on the network thread in `data_channel_network_thread.rs`.

#[allow(dead_code)] // read by the suite, which needs libwebrtc
const NETWORK_THREAD_CALLBACKS: bool = false;

include!("suites/data_channel.rs");
