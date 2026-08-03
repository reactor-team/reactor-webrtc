# reactor-webrtc

Safe, idiomatic Rust API over an owned build of Google's WebRTC engine
(`libwebrtc`). The low-level FFI lives in the `reactor-webrtc-sys` crate.

## Quick start

```toml
[dependencies]
reactor-webrtc = "0.2"
```

The build script automatically downloads the correct `libwebrtc` prebuilt for
your target when you run `cargo build` — no extra setup needed. The prebuilt is
cached in Cargo's build directory and not re-downloaded on subsequent builds.

To use a local build instead, set `REACTOR_WEBRTC_LIB_DIR=/path/to/prebuilt`.

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
| `SessionDescription` | SDP offer or answer; `ice_ufrags`, `with_ice_credentials` |
| `IceCandidate` | Trickled ICE candidate |
| `RtcConfiguration` | ICE servers + transport policy |
| `AdmMode` | `Synthetic` (push PCM) or `Platform` (real mic/speaker) |
| `ApmConfig` | AEC3, noise suppression, AGC, high-pass filter |

## Choosing your own ICE credentials

libwebrtc generates the ICE ufrag and password itself and exposes no setter — the
`SetIceParameters` entry point sits below the public API, and calling it out of
band would leave the transport disagreeing with the description that was
signalled. When something upstream needs to *recognise* a session by its ufrag —
an edge relay that demultiplexes on it, say — substitute the credentials in the
description before setting it locally:

```rust
let answer = pc.create_answer()?;
let answer = answer.with_ice_credentials(ufrag, pwd)?;
pc.set_local_description(&answer)?;

let ufrags = answer.ice_ufrags(); // one per m-section
```

This works because the local description is where libwebrtc reads the transport's
ICE parameters from, so the substituted values are the ones that reach the wire.
Every `a=ice-ufrag` and `a=ice-pwd` is replaced — bundled m-sections repeat the
attribute and have to agree — and nothing else is touched, `a=fingerprint`
included, so DTLS stays end-to-end.

Two things to get right:

- **Order.** Setting the local description is what creates the transport and
  starts gathering, so substituting afterwards acts on nothing.
- **Renegotiation.** Changing the credentials between generations *is* an ICE
  restart (RFC 8445 §9). On a renegotiation not meant to restart ICE, pass the
  values the session already uses; rotating a routing token out of habit would
  interrupt media.

Returns an error if a value falls outside RFC 8445's ranges (ufrag 4–256
characters, password 22–256) or contains anything outside `ice-char`
(`ALPHA / DIGIT / "+" / "/"`). That last check is also what stops a newline in a
credential from injecting an SDP line.

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
