//! The `dc_chunked` suite with the data channels' callbacks on the network
//! thread (`PeerConnectionFactoryBuilder::with_dc_callbacks_on_network_thread`).
//! Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test --test dc_chunked_network_thread
//! ```

#[allow(dead_code)] // read by the suite, which needs libwebrtc
const NETWORK_THREAD_CALLBACKS: bool = true;

include!("suites/dc_chunked.rs");
