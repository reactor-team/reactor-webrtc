# Video playout latency

How long a received video frame waits before it reaches the decoder, and the
three settings that change it.

- [What a default receiver does](#what-a-default-receiver-does)
- [Immediate playout](#immediate-playout): decode every frame as soon as it is
  complete — from the receiving side, or asked for by the sender.
- [A floor on the jitter buffer](#a-floor-on-the-jitter-buffer): hold frames
  *longer*, on one transceiver.
- [Measuring it](#measuring-it)

## What a default receiver does

libwebrtc schedules each frame against a render time: it waits out its jitter
estimate, plus a fixed ~10 ms of render-delay smoothing, before handing the
frame to the decoder. That suits watching video. For a control loop — a model
acting on frames from a robot's cameras — it is latency spent on nothing: on a
clean loopback, frames wait about 10–12 ms each.

The playout-delay RTP header extension
(`http://www.webrtc.org/experiments/rtp-hdrext/playout-delay`) bounds that
wait: the receiver keeps its delay between a `min` and a `max`. Both zero
([`PlayoutDelay::IMMEDIATE`]) switches the smoothing off, and frames go to the
decoder as soon as they are complete — in the same loopback, a few
microseconds.

## Immediate playout

Two factory knobs set it, one per end of the stream. Both are factory-wide:
libwebrtc reads them from field trials in the factory's environment, so they
cover every video stream the factory sends or receives.

| Knob | Set on | Effect |
|------|--------|--------|
| `with_receive_playout_delay` | the receiver | Plays every received stream within the limits, whatever the sender's extension says. Needs nothing from the sender. |
| `with_send_playout_delay` | the sender | Stamps the extension on every frame sent; any receiver that negotiated it (libwebrtc, and so every browser, does by default) honours it. |

Use the receive side when you own the receiver — it holds from the first frame
of every stream, whoever sent it. Use the send side when the receiver is
someone else's, such as a browser.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{PeerConnectionFactory, PlayoutDelay};

// Receiver: decode each frame as soon as it is complete.
let factory = PeerConnectionFactory::builder()
    .with_receive_playout_delay(PlayoutDelay::IMMEDIATE)
    .build()?;

// Sender: ask every receiver to do the same.
let factory = PeerConnectionFactory::builder()
    .with_send_playout_delay(PlayoutDelay::IMMEDIATE)
    .build()?;
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

b = rw.PeerConnectionFactoryBuilder()
b.with_receive_playout_delay()            # defaults: min_s=0, max_s=0
factory = b.build()

b = rw.PeerConnectionFactoryBuilder()
b.with_send_playout_delay(min_s=0.0, max_s=0.0)
factory = b.build()
```

</details>

Non-zero limits work too — `PlayoutDelay::new(min, max)` in Rust, `min_s` and
`max_s` in Python — up to the extension's 40.95 s, with `min <= max`.

Immediate playout trades smoothness for latency: with no render schedule,
frames go out as unevenly as the network delivered them, and a burst of loss
shows up as a stall instead of being absorbed.

## A floor on the jitter buffer

`Transceiver::set_jitter_buffer_minimum_delay` (the browser's
`RTCRtpReceiver.jitterBufferTarget`) holds that transceiver's received media at
least that long — up to 10 s. It is a floor: it can only add latency, which is
what you want for, say, smoothing a jittery link at the cost of a fixed delay.
`None` restores the default.

It is per transceiver and can be set before the stream exists, so it holds
from the first frame. Note that JSEP never reuses a transceiver from
`add_transceiver` for a remote offer's m-section: set it on a transceiver you
offer with, or on the one `on_track` hands you.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use std::time::Duration;

let rx = pc.add_transceiver(MediaKind::Video, TransceiverDirection::RecvOnly)?;
rx.set_jitter_buffer_minimum_delay(Some(Duration::from_millis(200)))?;
// ... create_offer() from this connection
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
rx = pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.RecvOnly)
await rx.set_jitter_buffer_minimum_delay(0.2)
```

</details>

## Measuring it

`get_stats` reports the jitter buffer on every inbound stream, cumulative over
the frames that have left it:

| Field | Meaning |
|-------|---------|
| `jitter_buffer_delay_s` | Time frames spent in the jitter buffer — for video, from a frame's first packet arriving to it leaving for the decoder. |
| `jitter_buffer_target_delay_s` | The delay the buffer aimed for, with every floor in force. A jitter buffer minimum shows up here, less the ~10 ms render delay. |
| `jitter_buffer_minimum_delay_s` | For video, libwebrtc's own computed minimum (jitter estimate plus decode and render time) — not a floor the app set. |
| `jitter_buffer_emitted_count` | Frames that have left the buffer: the denominator. |

`average_jitter_buffer_delay()` (Rust) / `average_jitter_buffer_delay_s`
(Python) divides the first by the last — the per-frame wait, which is the
number to compare before and after changing any of the above.

[`PlayoutDelay::IMMEDIATE`]: ../crates/reactor-webrtc/src/playout.rs
