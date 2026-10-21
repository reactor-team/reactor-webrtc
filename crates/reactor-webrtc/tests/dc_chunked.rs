//! Chunked data channels end to end: messages of any size arrive whole and
//! intact, the queue paces sends past libwebrtc's 16 MiB buffer, limits are
//! enforced, and a plain channel is untouched. Two peers on loopback. Run with:
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test --test dc_chunked -- --nocapture
//! ```
//!
//! Callbacks on the signaling thread, libwebrtc's default; the same suite runs
//! with them on the network thread in `dc_chunked_network_thread.rs`.

#[allow(dead_code)] // read by the suite, which needs libwebrtc
const NETWORK_THREAD_CALLBACKS: bool = false;

include!("suites/dc_chunked.rs");
