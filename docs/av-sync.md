# Audio/video sync

A receiver plays two streams together by matching their capture timestamps.
This page is about where those timestamps come from when the media is
synthetic — produced by a model, a renderer, or a file reader rather than by a
camera and a microphone on one clock.

## Why a synthetic source has to say when

The two tracks time themselves very differently.

**Video** carries a timestamp per frame. [`VideoTrack::push_frame`] reads
the clock at the moment of the call and stamps the frame with it, so a gap between frames is
self-describing: two frames 400 ms apart say so, and the receiver holds the
first for 400 ms.

**Audio** carries no timestamp of its own. The track's RTP timestamp is a
sample counter — the engine advances it by however many samples each
[`AudioTrack::push_frame`]
(on a `LocalPush` track) hands over. That says how much audio exists, never
when it happened.
Two consequences follow:

- A tick that pushes nothing is time the stream never accounts for. The packets
  either side of the gap stay contiguous in sequence number *and* timestamp, so
  the receiver cannot see that anything was skipped. It sees the whole stream
  arriving late and irregularly, and grows its jitter buffer to absorb what
  reads as network burstiness.
- Silence is the only way to express a gap. Pushing 480 zero samples says "10 ms
  passed with nothing in it"; pushing nothing says nothing at all.

So a synthetic source owes the audio track a frame on every tick, real or
silent, and owes both tracks a shared answer to *when was this captured*.

## Stamping both tracks from one clock read

`time_micros()` is the engine's monotonic clock, the epoch every
`capture_time_us` argument is read in. Read it once per unit of produced media
and give that one value to every track the unit spans:

```python
import reactor_webrtc as rw

now = rw.time_micros()
video.push_video_frame(bgra, width, height, capture_time_us=now)
audio.push_pcm(pcm, 48_000, 1, capture_time_us=now)
```

```rust
let now = reactor_webrtc::time_micros();
video.push_frame_at(VideoFrame::new(&bgra, width, height), now)?;
audio.push_frame_at(AudioFrame::new(&pcm, 48_000, 1), now)?;
```

Without the timestamps each track inherits the moment it happened to reach the
encoder. Those moments differ by however deep the buffers between the producer
and the wire are — and that depth moves, so the offset between the streams
moves with it. Sharing a capture time is what makes the alignment a property of
the media rather than of the plumbing.

The argument is optional everywhere. Omitting it keeps the old behaviour, which
is the right choice for a genuine live capture where the arrival moment *is* the
capture moment.

## Carrying metadata as well

A capture time and a metadata trailer are independent. A frame can carry both:

```python
video.push_video_frame(
    bgra, width, height,
    user_data=b"what the model saw",
    capture_time_us=now,
)
```

```rust
video.push_frame_with_metadata_at(VideoFrame::new(&bgra, width, height), b"...", now)?;
```

The trailer is matched to its frame by capture millisecond, so two frames
stamped inside the same millisecond would collide on one entry. The second is
nudged to the following millisecond — far below the resolution any
synchronisation cares about, and only reachable above 1000 fps. See
[frame-metadata.md](frame-metadata.md) for the trailer itself.

## Keeping the audio clock on the wall clock

Stamping the audio is necessary but not sufficient: the sample counter still has
to advance at the rate real time does. A producer feeding a track owes it a
frame per tick even when it has nothing to say.

```python
FRAME = 480                      # 10 ms at 48 kHz
SILENCE = b"\x00\x00" * FRAME

while running:
    chunk = buffer.take(FRAME)   # None when the producer is behind
    track.push_pcm(chunk or SILENCE, 48_000, 1, capture_time_us=rw.time_micros())
    sleep_until_next_tick()
```

Skipping the push when `chunk` is `None` is what produces the failure this page
opened with: the stream falls behind real time by exactly the time skipped, with
nothing on the wire to say so.

## How far a capture timestamp actually travels

The two tracks carry it very differently, and it is worth knowing which one is
doing the work.

**Video** puts the capture time straight into the RTP timestamp — video RTP
timestamps *are* capture times, at 90 kHz. `VideoTrack::push_frame_at`
therefore changes what goes on the wire unconditionally.

A frame that also carries [per-frame metadata](frame-metadata.md) carries the
capture time a second way, and the two differ in what they preserve. The RTP
timestamp is truncated to the millisecond and nudged forward when two frames of
one track share a millisecond, because it doubles as the key that pairs a frame
with its trailer. The trailer's own `capture_time_us` field is the value as
passed, so a receiver that needs the sender's exact number reads it there — and
several tracks stamped from one clock read all deliver that one number, which the
per-track RTP nudge cannot promise.

**Audio** does not. The RTP timestamp stays the sample counter, and the capture
time reaches the wire only through the `abs-capture-time` RTP header extension.
libwebrtc registers that extension as a capability on both engines but leaves it
at `RtpTransceiverDirection::kStopped`, so it is not offered by default —
and neither is it in Chrome. Until both peers negotiate it, an audio capture
timestamp is carried no further than the encoder.

The one exception is a resync: on the first frame after sending resumes,
`ChannelSend` advances the sample counter by the gap between that frame's
capture time and the previous one, so a stamped stream that pauses and restarts
resumes at the right offset instead of closing the gap up.

So stamping audio today is necessary but not yet sufficient. Negotiating
`abs-capture-time` needs `SetHeaderExtensionsToNegotiate` on both peers — the
offerer has to include the URI before the answerer is allowed to accept it
(RFC 8285), which in a browser-offers topology makes the client the gating side.

## What this does not do

Sharing a capture time aligns the two streams at the *source*. It does not
compensate for a receiver's own buffering choices, and it does not repair a
stream that under-produces: audio that arrives at 80% of real time will still be
stretched or concealed by the receiver whatever its timestamps say. Those are
producer-side problems, and the counters to find them belong to the producer.
