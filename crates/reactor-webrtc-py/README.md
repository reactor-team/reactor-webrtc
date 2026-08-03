# reactor-webrtc (Python)

Python bindings for the `reactor-webrtc` WebRTC engine, built with
[PyO3](https://pyo3.rs) and distributed as a self-contained wheel — no separate
native library required at runtime.

## Installation

```bash
pip install reactor-webrtc
```

Requires **Python ≥ 3.10**.

## Quick start

```python
import reactor_webrtc as rw

factory = rw.PeerConnectionFactory()

obs = rw.PeerConnectionObserver()
obs.on_ice_candidate = lambda c: relay_to_peer(c)
obs.on_connection_state_change = lambda s: print("state:", s)

config = rw.RtcConfiguration(ice_servers=[
    rw.IceServer(urls=["stun:stun.l.google.com:19302"]),
    # A turn:/turns: entry needs both credentials, or libwebrtc rejects the
    # whole configuration.
    rw.IceServer(urls=["turn:turn.example.com:3478"], username="alice", password="secret"),
])
pc = factory.create_peer_connection(config, obs)

offer = pc.create_offer()
pc.set_local_description(offer)

# Exchange offer.sdp with the remote peer via your signaling channel, then:
# pc.set_remote_description(remote_answer)
# pc.add_ice_candidate(candidate)
```

## Audio

```python
# Headless / server: push PCM programmatically (synthetic ADM, default)
factory = rw.PeerConnectionFactory()
factory.push_audio_frame(pcm_bytes, sample_rate=48000, channels=1)

# Desktop client: real mic + AEC3 + noise suppression
factory = rw.PeerConnectionFactory(
    platform_adm=True,
    echo_canceller=True,
    noise_suppression=True,
)
```

## Pre-encoded video

```python
factory, video = rw.PeerConnectionFactory.with_encoded_video_track(
    "camera", width=1280, height=720
)
pc = factory.create_peer_connection(config, obs)
tx = video.add_transceiver(pc, rw.TransceiverDirection.SendOnly)

# From your encoder thread:
video.push_encoded_frame(
    data=h264_annex_b,
    is_key_frame=True,
    width=1280, height=720,
)
```

## Receiving media

```python
obs = rw.PeerConnectionObserver()

def on_track(kind, track):
    if kind == rw.MediaKind.Video:
        track.on_video_frame(lambda bgra, w, h: display(bgra, w, h))
    elif kind == rw.MediaKind.Audio:
        track.on_audio_frame(lambda pcm, sr, ch, n: play(pcm))

obs.on_track = on_track
```

## Stats

```python
report = pc.get_stats()
for pair in report.candidate_pairs:
    print(pair.state, f"{pair.current_round_trip_time_s * 1000:.1f}ms")
```

## API reference

| Class | Description |
|-------|-------------|
| `PeerConnectionFactory` | Entry point; creates peer connections and tracks |
| `PeerConnection` | SDP offer/answer, ICE, tracks, data channels, stats |
| `PeerConnectionObserver` | Callbacks: state, ICE candidate, track, data channel |
| `RtcConfiguration` | ICE servers, ICE transport type, gathering policy |
| `IceServer` | A STUN or TURN server entry |
| `IceCandidate` | A trickled ICE candidate |
| `SessionDescription` | SDP offer or answer (`kind`, `sdp`, `ice_ufrags`, `with_ice_credentials`) |
| `Track` | Local (push frames) or remote (attach sink) media track |
| `EncodedVideoTrack` | Push pre-encoded video (H.264 Annex-B, VP8, VP9, …) |
| `Transceiver` | RTP m-section: `mid`, `kind`, `set_track`, `set_direction` |
| `DataChannel` | SCTP data channel: `send`, `on_message`, `on_open`, … |
| `StatsReport` | `inbound_rtp`, `outbound_rtp`, `candidate_pairs` |

| Enum | Values |
|------|--------|
| `PeerConnectionState` | `New`, `Connecting`, `Connected`, `Disconnected`, `Failed`, `Closed` |
| `IceGatheringState` | `New`, `Gathering`, `Complete` |
| `TransceiverDirection` | `SendRecv`, `SendOnly`, `RecvOnly`, `Inactive` |
| `MediaKind` | `Audio`, `Video` |
| `DataChannelState` | `Connecting`, `Open`, `Closing`, `Closed` |
| `IceCandidatePairState` | `Waiting`, `InProgress`, `Failed`, `Succeeded`, `Cancelled` |

| String-valued field | Values |
|---------------------|--------|
| `RtcConfiguration.ice_transport_type` | `all` (default), `relay`, `no_host`, `none` |
| `RtcConfiguration.continual_gathering_policy` | `once` (default), `continually` |

## Choosing your own ICE credentials

libwebrtc generates the ICE ufrag and password itself and offers no setter. If
something upstream needs to *recognise* a session by its ufrag — an edge relay
that demultiplexes on it, say — substitute the credentials in the description
before setting it locally:

```python
answer = pc.create_answer()
answer = answer.with_ice_credentials(my_ufrag, my_password)
pc.set_local_description(answer)

answer.ice_ufrags()  # ["<my_ufrag>", ...] — one per m-section
```

The local description is what libwebrtc reads the transport's ICE parameters
from, so the substituted values are the ones that end up on the wire.

Two things to get right:

- **Order.** Setting the local description is what creates the transport and
  starts gathering, so substituting afterwards acts on nothing.
- **Renegotiation.** Changing the credentials between generations *is* an ICE
  restart (RFC 8445 §9). On a renegotiation that is not meant to restart ICE,
  pass the values the session already uses.

Raises if a value is outside RFC 8445's ranges (ufrag 4–256 characters, password
22–256) or contains anything outside `ice-char` — which is also what stops a
newline in a credential from injecting an SDP line.

## Thread safety

Signaling methods (`create_offer`, `set_local_description`, etc.) block for a
few milliseconds while the WebRTC engine responds. Wrap in `asyncio.to_thread()`
when calling from an async context. Callbacks fire on WebRTC internal threads
with the GIL acquired; keep them fast.

## License

Apache-2.0. Upstream WebRTC is BSD-3-Clause + the WebRTC patent grant.
