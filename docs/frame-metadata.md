# Per-frame metadata and encoded-frame transforms

A way to ride your own data alongside a video frame on the wire, and a
lower-level hook to inspect or rewrite an encoded frame as it passes through
the sender or receiver path.

- [Basic usage](#basic-usage): push `user_data` with a frame, read it back as
  `metadata` on the other side — nothing else to wire up.
- [How it's negotiated](#how-its-negotiated): why nothing has to be wired up.
- [Checking whether the peer agreed](#checking-whether-the-peer-agreed)
- [Turning it off](#turning-it-off)
- [Custom encoded-frame transforms](#custom-encoded-frame-transforms): a
  callback that sees every encoded frame and can forward, drop, or rewrite
  it, composing with the metadata step rather than replacing it.

Examples are given in Rust and Python; the underlying mechanism is the same.

## Basic usage

Every video track — raw (`Track`) or pre-encoded (`EncodedVideoTrack`) — can
embed arbitrary bytes in the packet trailer on the way out, and recover them
on the way in. There is nothing to attach: push with `user_data`, read
`metadata` off the decoded frame.

<details>
<summary>🦀 Example using Rust</summary>

```rust
track.push_video_frame_with_metadata(&bgra, width, height, b"anything you like");

track.on_video_frame(|frame| {
    if let Some(metadata) = &frame.metadata {
        println!("{} {} {:?}", metadata.frame_id, metadata.timestamp, metadata.user_data);
    }
});
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
track.push_video_frame(bgra, width, height, user_data=b"anything you like")

def on_frame(bgra, width, height, metadata):
    if metadata is not None:
        print(metadata.frame_id, metadata.timestamp, metadata.user_data)

track.on_video_frame(on_frame)
```

</details>

`EncodedVideoTrack` takes the same `user_data` argument on
`push_encoded_frame_with_metadata` (Rust) / `push_encoded_frame(...,
user_data=...)` (Python).

`metadata`/`frame.metadata` is `None` whenever the far peer hasn't agreed to
strip the trailer (see below), or the sender didn't include one for that
particular frame — always check for `None` rather than assuming it's
present.

Python's `on_video_frame` also accepts the legacy 3-argument signature
(`callback(bgra, width, height)`) for code that predates this feature; if a
4-argument callback raises, it is retried once as a 3-argument call. The
Rust closure always takes one `VideoFrame` struct, so there's no equivalent
legacy shape.

## How it's negotiated

Appending a trailer only helps if the far end knows to strip it before its
decoder sees the extra bytes. That capability is negotiated automatically —
you never check what the peer supports, you just push `user_data` and it's
included or silently dropped:

- `create_offer` advertises the capability as a session-level SDP attribute
  on every offer.
- `create_answer` mirrors an offer that asked for it.
- `set_remote_description` arms the connection's gate from that attribute
  and, once open, wires the embed/strip steps into the video transceivers
  automatically — there is no transform to build or attach yourself.

A peer that has never heard of the attribute ignores it (per RFC 8866, an
unrecognized SDP attribute is ignored, not rejected), the gate stays closed,
and `user_data` is silently dropped rather than corrupting that peer's
decode. The declaration is session-level, not per-track — an audio-only
offer already carries it, so a later renegotiation that adds video doesn't
need to introduce the capability mid-session.

This replaces an earlier version of this API that required attaching a
`sender_metadata_transform()`/`receiver_metadata_transform()` by hand before
the first SDP exchange. Those methods no longer exist — negotiation and
installation are both automatic now.

## Checking whether the peer agreed

Reading it is useful for diagnostics; you don't need to check it before
pushing `user_data`.

<details>
<summary>🦀 Example using Rust</summary>

```rust
let gate = pc.frame_metadata_gate();
if gate.is_open() {
    // the peer agreed; trailers are being appended/stripped
}
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
if pc.frame_metadata_gate().is_open():
    pass  # the peer agreed; trailers are being appended/stripped
```

</details>

## Turning it off

`RtcConfiguration.frame_metadata` (default `true`) keeps the capability out
of a connection entirely: offers don't advertise it, answers stay silent
even when the offer declared it, the gate never opens, and `user_data` is
dropped. The result is indistinguishable from a connection built before the
capability existed.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::RtcConfiguration;

let config = RtcConfiguration { frame_metadata: false, ..Default::default() };
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(frame_metadata=False)
```

</details>

Reasons to: a peer whose encoded payloads must be byte-identical to what the
encoder produced, a deployment that hasn't rolled the capability out to both
ends yet, or ruling frame metadata out while bisecting something else.

## Custom encoded-frame transforms

`FrameTransform` also takes an arbitrary callback for cases the built-in
metadata trailer doesn't cover — inspecting compressed frame bytes, dropping
frames under some condition, or rewriting the payload outright. It composes
with the frame-metadata step rather than displacing it: your callback runs
first, on exactly the bytes that traverse the network — before a trailer is
appended on send, before one is stripped on receive — and the metadata step
runs after. Attaching your own transform never disables metadata, and vice
versa.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{FrameAction, FrameTransform};

let transform = FrameTransform::new(|frame| {
    if frame.is_key_frame {
        log_keyframe(frame.ssrc, frame.timestamp);
    }
    if should_drop(frame) {
        return FrameAction::Drop;
    }
    FrameAction::Forward
});
transceiver.set_sender_transform(&transform)?;
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

def inspect(frame: rw.EncodedFrame) -> rw.FrameAction:
    if frame.is_key_frame:
        log_keyframe(frame.ssrc, frame.timestamp)
    if should_drop(frame):
        return rw.FrameAction.Drop
    return rw.FrameAction.Forward

transceiver.set_sender_transform(rw.FrameTransform(inspect))
```

</details>

To rewrite a frame's payload, call `replace_data` on the frame *inside* the
callback, then return `Forward` — the new bytes are what gets sent (or
delivered to the decoder, on a receiver transform). If you want the payload
without the metadata framing, apply `reactor_webrtc::metadata::decode_and_strip_trailer`
yourself — your callback sees the trailer still attached, since it runs
before the crate's own strip step.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{FrameAction, FrameTransform};

let transform = FrameTransform::new(|frame| {
    frame.replace_data(&scrub(frame.data));
    FrameAction::Forward
});
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

def redact(frame: rw.EncodedFrame) -> rw.FrameAction:
    frame.replace_data(scrub(frame.data))
    return rw.FrameAction.Forward
```

</details>

Where the transform runs in the pipeline:

- **Sender transform** — after the encoder, before RTP packetization (and
  before the metadata trailer, if any, is appended).
- **Receiver transform** — after RTP depacketization, before the decoder
  (and before the metadata trailer, if any, is stripped).

Unlike frame metadata, a transform is a real per-transceiver registration:
calling `set_sender_transform`/`set_receiver_transform` again on the same
transceiver replaces the previous callback.
