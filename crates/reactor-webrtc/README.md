# reactor-webrtc

Safe, idiomatic Rust API over an owned build of Google's WebRTC engine
(`libwebrtc`). The low-level FFI lives in the `reactor-webrtc-sys` crate.

## Quick start

```sh
cargo add reactor-webrtc
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
| `Transceiver` | RTP send/recv direction + MID; `set_codec_preferences`/`lock_negotiated_send_codec` for video codec choice |
| `StatsReport` | `inbound_rtp`, `outbound_rtp`, `candidate_pairs` |
| `SessionDescription` | SDP offer or answer; `ice_ufrags`, `with_ice_credentials`, `declares_frame_metadata`, `with_frame_metadata` |
| `FrameMetadataGate` | What the remote declared about per-frame metadata |
| `IceCandidate` | Trickled ICE candidate |
| `RtcConfiguration` | ICE servers/policy, port range, bundle policy, TCP candidates, ICE timeouts — see [`docs/configuration.md`](../../docs/configuration.md) |
| `AdmMode` | `Synthetic` (push PCM) or `Platform` (real mic/speaker) |
| `ApmConfig` | AEC3, noise suppression, AGC, high-pass filter |

`PeerConnection::set_bitrate` sets congestion-control bitrate limits after
the connection is created — also covered in
[`docs/configuration.md`](../../docs/configuration.md). For per-frame
metadata and custom encoded-frame transforms, see
[`docs/frame-metadata.md`](../../docs/frame-metadata.md).

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

## Codec preferences

```rust
let tx = pc.add_transceiver(MediaKind::Video, TransceiverDirection::SendOnly)?;
tx.set_track(&video_track)?;
tx.set_codec_preferences(&[VideoCodec::Vp9, VideoCodec::Vp8])?;

let answer = pc.create_answer()?;
pc.set_local_description(&answer)?;

// Only after set_local_description does the sender's negotiated codec list
// exist to lock onto — set_codec_preferences alone shapes the SDP, not which
// negotiated codec this side's own sender actually encodes with.
tx.lock_negotiated_send_codec()?;
```

`set_codec_preferences` reorders the transceiver's negotiable codecs so the
given ones sort first — a codec this build does not actually support (no
hardware H.264, say) is silently skipped rather than rejected. Nothing else
is dropped: unlisted codecs, and every retransmission/RED/FEC entry, keep
their original relative order after the preferred ones.

## Per-frame metadata

Arbitrary bytes can ride alongside each encoded video frame, in a protobuf trailer
appended to the payload:

```text
[ encoded payload ][ proto bytes ][ u32 LE: proto_len ][ b"RXMT" ]
```

Push it with the frame; read it off the decoded one:

```rust
track.push_video_frame_with_metadata(&bgra, w, h, b"anything you like");

track.on_video_frame(|frame| {
    if let Some(meta) = frame.metadata {
        // meta.user_data, meta.frame_id, meta.timestamp
    }
});
```

That only works if the far end strips the trailer before its decoder sees it, so
support is negotiated in the SDP and **you do not have to do anything for it** —
there are no transforms to build or attach:

- `create_offer` advertises the capability as one session-level
  `a=x-reactor-frame-metadata:1`, inserted before the first media section.
- `create_answer` mirrors an offer that asked for it.
- `set_remote_description` arms the connection's `FrameMetadataGate` and, when it
  is open, installs the embed and strip transforms on the video transceivers. The
  remote track exists by then: libwebrtc creates it while applying the
  description, which is the same point `on_track` fires.
- The sender transform still consults the gate per frame, so a renegotiation in
  which the peer drops support stops the trailers without detaching anything.

A peer that has never heard of the attribute ignores it — RFC 8866 §6 requires
unrecognised attributes to be ignored — and the gate stays closed, so `user_data`
is silently dropped rather than corrupting that peer's decode. Check
`pc.frame_metadata_gate().is_open()` if you want to know whether the peer agreed.

Three details worth knowing:

- **Session level, not per m-section.** Understanding the trailer is a property of
  a peer's code, not of one of its tracks, so one line covers the session — audio-only
  descriptions included, which keeps a renegotiation that adds video from having to
  introduce the capability mid-session.
- **Read it from the SDP string.** libwebrtc discards `a=` lines it does not
  recognise while parsing, so the declaration is only ever visible in the signalled
  text — not in anything the stack hands back. Nothing in this crate depends on the
  parsed form. Browsers behave the same way, so a browser peer must read the raw
  signalled SDP too.
- **The version is the compatibility token.** An incompatible change to the trailer
  format bumps `FRAME_METADATA_VERSION`, and old and new then never agree.

An `a=extmap` would have been the recognisable spelling, and was the first thing
tried, but it means "I will send this RTP header extension" — which is not true, no
header extension is ever emitted — and it drags in a shared id namespace that the
*peer* validates (RFC 8843: one id, one URI, across a BUNDLE group), so a collision
would surface as the far side's `set_remote_description` failing. An unregistered
`x-` attribute claims nothing false and has no id to collide.
- **Your own `FrameTransform` composes with it.** libwebrtc holds one transformer
  per sender and per receiver, so the crate owns those slots and runs both things
  that want them. Your callback goes first in both directions, so it sees exactly
  the bytes that traverse the network — before a trailer is appended on send,
  before one is stripped on receive. Apply `metadata::decode_and_strip_trailer`
  yourself if you want the payload without the framing.

To keep frame metadata out of a connection entirely — no `a=extmap`, no mirroring,
no transforms, `user_data` dropped — build it with the capability off:

```rust
let config = RtcConfiguration { frame_metadata: false, ..Default::default() };
```

Worth doing for a peer whose encoded payloads must be byte-identical to the
encoder's output, a rollout that has not reached both ends, or to rule frame
metadata out while bisecting something else.

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
