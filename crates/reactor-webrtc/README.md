# reactor-webrtc

Safe, idiomatic Rust API over an owned build of Google's WebRTC engine
(`libwebrtc`). The wheel embeds the native library statically — no separate
runtime dependency.

## Quick start

```toml
[dependencies]
reactor-webrtc = "0.1"
```

A native `libwebrtc` must be available at link time — set one of:

- `REACTOR_WEBRTC_LIB_DIR=/path/to/extracted-prebuilt` — local build or
  extracted prebuilt archive.
- `REACTOR_WEBRTC_PREBUILT_URL=https://…` (+ `REACTOR_WEBRTC_PREBUILT_SHA256`)
  — download the prebuilt for the current target at build time.

Without either variable set, `cargo check` and rlib builds of the API succeed;
only the final link of a binary or test requires the native library.

## Example

```rust
use reactor_webrtc::{
    PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration,
};

let factory = PeerConnectionFactory::new()?;

let obs = PeerConnectionObserver::new()
    .on_ice_candidate(|c| println!("ICE: {:?}", c.candidate))
    .on_connection_state_change(|s| println!("state: {:?}", s));

let config = RtcConfiguration::default();
let pc = factory.create_peer_connection(&config, obs)?;

let offer = pc.create_offer()?;
pc.set_local_description(&offer)?;

// Exchange offer/answer with the remote peer via your signaling channel, then:
// pc.set_remote_description(&answer)?;
// pc.add_ice_candidate(&candidate)?;
```

## API surface

| Type | Description |
|------|-------------|
| `PeerConnectionFactory` | Entry point — creates peer connections and tracks |
| `PeerConnection` | Offer/answer, ICE, tracks, data channels, stats |
| `PeerConnectionObserver` | Callbacks: state change, ICE candidate, track, data channel |
| `DataChannel` | Reliable/unreliable messaging over SCTP |
| `Track` | Local (push frames) or remote (attach a sink) media track |
| `EncodedVideoTrack` | Push pre-encoded video frames (H.264, VP8, VP9, …) |
| `Transceiver` | RTP send/recv direction + MID |
| `StatsReport` | `inbound_rtp`, `outbound_rtp`, `candidate_pairs` |
| `SessionDescription` | SDP offer or answer |
| `IceCandidate` | Trickled ICE candidate |
| `RtcConfiguration` | ICE servers + transport policy |
| `AdmMode` | `Synthetic` (push PCM) or `Platform` (real mic/speaker) |
| `ApmConfig` | AEC3, noise suppression, AGC, high-pass filter |

## Audio modes

```rust
// Headless / server: push PCM programmatically
let factory = PeerConnectionFactory::new()?;  // synthetic ADM (default)
factory.push_audio_frame(&pcm_i16, 48000, 1);

// Desktop client: real mic + AEC3 + noise suppression + AGC
let factory = PeerConnectionFactory::with_platform_adm()?;
```

## Pre-encoded video

```rust
let (factory, video) =
    PeerConnectionFactory::with_encoded_video_track("cam", 1280, 720)?;

let pc = factory.create_peer_connection(&config, observer)?;
let tx = pc.add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)?;
tx.set_track(video.track())?;

// From your encoder thread:
video.push_encoded_frame(EncodedVideoFrame {
    data: h264_annex_b,
    is_key_frame: true,
    width: 1280, height: 720,
    rtp_timestamp: 0,
});
```

## Target support

| Platform | Architecture | Notes |
|----------|-------------|-------|
| macOS | arm64, x64 | VideoToolbox H.264 |
| iOS | arm64 (device + simulator) | |
| Linux | x64, arm64 | bundled libc++ (ABI `__Cr`) |
| Android | arm64 | NDK + bundled libc++ |
| Windows | x64 | MSVC STL |

## License

Apache-2.0 — see [LICENSE](../../LICENSE).

Upstream WebRTC is BSD-3-Clause + the WebRTC patent grant; attribution recorded
in the SBOM published with every prebuilt release.
