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

`PeerConnection`'s signaling methods are natively awaitable, so this runs
inside an `asyncio` event loop:

```python
import asyncio
import reactor_webrtc as rw

async def main():
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

    offer = await pc.create_offer()
    await pc.set_local_description(offer)

    # Exchange offer.sdp with the remote peer via your signaling channel, then:
    # await pc.set_remote_description(remote_answer)
    # await pc.add_ice_candidate(candidate)

asyncio.run(main())
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

## Codec preferences

```python
tx = pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
await tx.set_track(video_track)
await tx.set_codec_preferences([rw.VideoCodec.Vp9, rw.VideoCodec.Vp8])

answer = await pc.create_answer()
await pc.set_local_description(answer)
```

`set_local_description`/`set_remote_description` also make this
transceiver's own sender actually encode with whichever preferred codec was
negotiated, once negotiation completes — not just list it first in the SDP.
No further call needed.

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
report = await pc.get_stats()
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
| `SessionDescription` | SDP offer or answer (`kind`, `sdp`, `ice_ufrags`, `with_ice_credentials`, `declares_frame_metadata`, `with_frame_metadata`) |
| `FrameMetadata` | Per-frame `frame_id`, `timestamp`, `user_data` |
| `FrameMetadataGate` | What the remote declared about per-frame metadata |
| `Track` | Local (push frames) or remote (attach sink) media track |
| `EncodedVideoTrack` | Push pre-encoded video (H.264 Annex-B, VP8, VP9, …) |
| `Transceiver` | RTP m-section: `mid`, `kind`, `set_track`, `set_direction`, `set_codec_preferences`, `set_sender_transform`, `set_receiver_transform` |
| `DataChannel` | SCTP data channel: `send`, `on_message`, `on_open`, … |
| `StatsReport` | `inbound_rtp`, `outbound_rtp`, `candidate_pairs` |
| `FrameMetadata`, `FrameAction`, `EncodedFrame`, `FrameTransform` | Per-frame metadata trailers and custom encoded-frame transforms — see [`docs/frame-metadata.md`](../../docs/frame-metadata.md) |

| Enum | Values |
|------|--------|
| `PeerConnectionState` | `New`, `Connecting`, `Connected`, `Disconnected`, `Failed`, `Closed` |
| `IceGatheringState` | `New`, `Gathering`, `Complete` |
| `TransceiverDirection` | `SendRecv`, `SendOnly`, `RecvOnly`, `Inactive` |
| `VideoCodec` | `Vp8`, `Vp9`, `Av1`, `H264`, `H265` |
| `MediaKind` | `Audio`, `Video` |
| `DataChannelState` | `Connecting`, `Open`, `Closing`, `Closed` |
| `IceCandidatePairState` | `Waiting`, `InProgress`, `Failed`, `Succeeded`, `Cancelled` |

| String-valued field | Values |
|---------------------|--------|
| `RtcConfiguration.ice_transport_type` | `all` (default), `relay`, `no_host`, `none` |
| `RtcConfiguration.continual_gathering_policy` | `once` (default), `continually` |
| `RtcConfiguration.bundle_policy` | `Balanced` (default), `MaxBundle`, `MaxCompat` |
| `RtcConfiguration.tcp_candidate_policy` | `Disabled` (default), `Enabled` |

`RtcConfiguration` also takes a `min_port`/`max_port` pair (UDP port range),
`ice_connection_receiving_timeout_ms`, and
`ice_check_interval_strong_connectivity_ms`; `PeerConnection.set_bitrate`
sets congestion-control bitrate limits after the connection is created. All
covered in [`docs/configuration.md`](../../docs/configuration.md).

## Per-frame metadata

Arbitrary bytes can ride alongside each encoded video frame, in a protobuf
trailer appended to the payload:

```python
video.push_video_frame(bgra, 320, 240, user_data=b"anything you like")

def on_frame(bgra, w, h, meta):
    if meta is not None:
        print(meta.frame_id, meta.timestamp, meta.user_data)

track.on_video_frame(on_frame)
```

That only works if the far end strips the trailer before its decoder sees it, so
support is negotiated in the SDP and **you do not have to do anything for it**:

- `create_offer` advertises the capability as one session-level
  `a=x-reactor-frame-metadata:1` (`rw.FRAME_METADATA_ATTRIBUTE`,
  `rw.FRAME_METADATA_VERSION`).
- `create_answer` mirrors an offer that asked for it.
- `set_remote_description` arms `pc.frame_metadata_gate()` and, when it is open,
  installs the embed and strip transforms on the video transceivers. The sender
  transform still checks the gate per frame, so a renegotiation that drops support
  stops the trailers.

A peer that has never heard of the attribute ignores it, the gate stays closed,
and `user_data` is silently dropped rather than corrupting that peer's decode.
Read `pc.frame_metadata_gate().is_open()` if you want to know whether the peer
agreed.

Read the declaration from the signalled SDP string, not from `pc.remoteDescription`
or its equivalents: libwebrtc and browsers both discard `a=` lines they do not
recognise while parsing.

A `FrameTransform` of your own composes with the metadata step rather than
displacing it — the library owns libwebrtc's single transformer slot per
sender/receiver and runs both. Your callback goes first in both directions, so it
sees exactly the bytes that traverse the network.

To keep frame metadata out of a connection entirely:

```python
config = rw.RtcConfiguration(frame_metadata=False)
```

No `a=extmap`, no mirroring, no transforms, and `user_data` is dropped — the
connection is indistinguishable from one built before the capability existed.

## Choosing your own ICE credentials

libwebrtc generates the ICE ufrag and password itself and offers no setter. If
something upstream needs to *recognise* a session by its ufrag — an edge relay
that demultiplexes on it, say — substitute the credentials in the description
before setting it locally:

```python
answer = await pc.create_answer()
answer = answer.with_ice_credentials(my_ufrag, my_password)
await pc.set_local_description(answer)

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

`PeerConnection`'s signaling methods (`create_offer`, `create_answer`,
`set_local_description`, `set_remote_description`, `add_ice_candidate`,
`get_stats`, `transceivers`) plus `set_bitrate` are natively awaitable —
`await` them directly, no `asyncio.to_thread()`/executor wrapping needed.
They still take a few milliseconds to resolve while the WebRTC engine
responds, but that wait happens off the event loop thread, so it never
blocks other coroutines. On `Transceiver`, `set_direction`, `set_track`,
and `set_codec_preferences` are the same way.

Every other method (`add_track`, `add_transceiver`, `create_data_channel`,
`Transceiver.set_sender_transform`/`set_receiver_transform`, and everything
on `Track`/`DataChannel`) is a fast synchronous call with no native
round-trip, and stays a plain function call — no `await`.

Callbacks fire on WebRTC internal threads with the GIL acquired; keep them
fast.

## License

Apache-2.0. Upstream WebRTC is BSD-3-Clause + the WebRTC patent grant.
