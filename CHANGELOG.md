# Changelog

## 0.15.0 — the stats report says which stream is which

`get_stats` reported a subset of libwebrtc's report, and the subset was narrower
than a client needs. Building `get_stats()` for the Reactor Python and C++ SDKs
turned up four fields those SDKs could not fill and one absence that forced them
to answer a different question than the browser does.

Additive except for two counters that widen from `u32` to `u64` — see Fixed.
Nothing removed, nothing renamed.

### Added

**`kind` on both stream types** (`RTCRtpStreamStats::kind`, as `StreamKind`).
The one that changes an answer rather than adding one. Without it a reader has
only the SSRC and cannot tell which of several receive streams is the video one
— so "the video stream's jitter" was not a question that could be asked, and
consumers aggregated across every stream instead.

**Most of the candidate pair.** `nominated` is the pair ICE actually selected;
before it, "selected" had to be inferred from `state` plus `priority`. Also
`writable`, `total_round_trip_time_s`, the pair's own `bytes_sent` /
`bytes_received` / `packets_sent` / `packets_received`, and
`available_outgoing_bitrate_bps` / `available_incoming_bitrate_bps` — the
congestion controller's own estimates, which nothing else substitutes for.

**`local_candidate_type` and `local_relay_protocol`** (`IceCandidateType`,
`RelayProtocol`), resolved through the pair's `local_candidate_id`.
`IceCandidateType::Relay` is what says a session is going through TURN, which is
the first thing worth knowing when latency is bad. It lives on a stat type the
glue did not visit at all.

**Frame counters.** `frames_per_second`, `frame_width` / `frame_height`, plus
`frames_decoded` / `frames_dropped` inbound and `frames_sent` outbound.

### Fixed

**Three packet counters were narrowed to 32 bits and wrapped silently.**
libwebrtc reports `RTCSentRtpStreamStats::packets_sent`,
`RTCOutboundRtpStreamStats::retransmitted_packets_sent` and the candidate
pair's `packets_sent` / `packets_received` as `uint64_t`; the C ABI struct
declared all of them `uint32_t`. A connection carrying a thousand packets a
second passes 2^32 in about seven weeks, after which a cumulative counter
appears to go backwards — the one thing a cumulative counter must not do.

`OutboundRtpStats::packets_sent` and
`OutboundRtpStats::retransmitted_packets_sent` are therefore now `u64`. That is
this release's only breaking change, and it is a widening: `let n: u64 =
stats.packets_sent` keeps working, `let n: u32` does not.
`InboundRtpStats::packets_received` stays `u32`, because
`RTCReceivedRtpStreamStats` genuinely reports 32 bits there.

**`OutboundRtpStats::round_trip_time_s` was a hard `0.0`.** libwebrtc moved the
send path's RTT out of `RTCOutboundRtpStreamStats` in M7907 and into the
receiver's report about us (`RTCRemoteInboundRtpStreamStats`); the field stayed
declared and stopped being assigned. Every caller reading it since that
milestone bump got a zero, which reads as a zero-latency link rather than as a
missing measurement.

Now followed through `remote_id`, along with `total_round_trip_time_s`,
`fraction_lost` and `packets_lost` from the same report — the send path's loss
numbers, equally absent before. It still reports `0.0` until the far end has
sent an RTCP receiver report, so a zero means "not measured yet"; the loopback
test waits for the value rather than for a frame count, because a lookup that
matches nothing is exactly the failure this had.

### Notes

The report's C ABI struct grew, and `glue/reactor_webrtc.cpp` is compiled from
source on every build, so no ABI version changed. What does need care is the
hand-written `repr(C)` mirror in `reactor-webrtc-sys`: both copies now carry a
size assertion against the same number, so editing one without the other fails
the build instead of shifting every field after it.

## 0.14.1 — say when a sender has encodings to write

`set_send_bitrate` documented itself as callable "before or after negotiation".
That is true of a transceiver from `add_transceiver`, which seeds a default
encoding, and false of the shape every answerer actually has:

| Transceiver from | audio | video |
|---|---|---|
| `add_transceiver` | has encodings | has encodings |
| applying a remote description | **none until the local description is applied** | has encodings |

An answerer that bounds its audio senders while building the answer gets
`sender has no encodings`, which took down a whole negotiation in
reactor-runtime before the cause was clear.

The refusal now names the cause and when the call becomes valid, rather than
leaving the reader to inspect their own track plumbing.

Only the *default* being lifted is video-specific — it is keyed on frame size.
The bounds themselves apply to an audio sender too, capping its allocation. So
an answerer that only wants to clear that default can bound its video senders
while building the answer; one that also wants an audio bound applies it after
`set_local_description`.

Docs corrected in the Rust API, the C ABI and `docs/configuration.md`. No API
change.

## 0.14.0 — per-sender bitrate bounds

`PeerConnection::set_bitrate` bounds the congestion controller's estimate for
the whole connection. Nothing bounded a single sender, and that is the ceiling
which actually caps a video encoder: with no explicit per-stream maximum,
libwebrtc derives one from the frame size alone, and it is **2500 kbps for
anything above 960x540**. 720p, 1080p and 4K all cap at 2.5 Mbps, and no amount
of congestion-control headroom lifts it — the two ceilings are conjunctive, so
the lower one wins.

Additive. Nothing removed, nothing renamed.

### Added

`Transceiver::set_send_bitrate(min_bps, max_bps)` — sets
`RtpEncodingParameters::min/max_bitrate_bps` on the sender's first encoding.
Callable before or after negotiation, and again to change the bounds mid-call.

```rust
// Let a 1080p track use up to 8 Mbps instead of libwebrtc's 2.5.
transceiver.set_send_bitrate(None, Some(8_000_000))?;
```

```python
await transceiver.set_send_bitrate(max_bps=8_000_000)
```

Both bounds are optional; `None` leaves one at the libwebrtc default. A
negative value is refused rather than read as "unset" — `None` is how a bound
is left alone, so a negative is far likelier to be a typo or an arithmetic slip
than a request, and quietly removing a cap somebody set would be the opposite
of what was asked. `min_bps` above `max_bps` is refused too, with a message
naming the pair, which libwebrtc's own rejection does not.

The C ABI gains `reactor_webrtc_rtp_transceiver_set_send_bitrate`, where `-1`
— and only `-1` — spells "leave at the default".

See the new "Per-sender bitrate limits" section in
[`docs/configuration.md`](docs/configuration.md), which spells out how this
ceiling relates to `set_bitrate`'s. They are easy to confuse and only one of
them lifts the default.

## 0.13.0 — composable factory builder + per-track options

The factory learns a single composable builder, tracks learn per-track
options, and everything semantic moved out of the factory constructors
into `create_*_track` calls — where it always belonged.

Rust is a breaking release across most of the factory/track surface
(everything below). **Python keeps its existing API entirely** — the same
names keep working (`push_video_frame`, `push_pcm`, `on_video_frame`, …);
the new capabilities are additions only. See the Python paragraph after
the Rust table.

### Removed in Rust (with the exact replacement)

| Removed | Replacement |
|---|---|
| `PeerConnectionFactory::new()` | `PeerConnectionFactory::builder().build()?` |
| `PeerConnectionFactory::with_adm(mode)` | `builder().with_adm(mode).build()?` |
| `PeerConnectionFactory::with_adm_apm(mode, apm)` | `builder().with_adm(mode).with_apm(apm).build()?` |
| `PeerConnectionFactory::with_platform_adm()` | `builder().with_platform_adm().build()?` |
| `PeerConnectionFactory::with_openh264(path, mode, apm)` | `builder().with_openh264(&path).build()?` |
| `PeerConnectionFactory::with_custom_video_encoder(enc)` | `create_video_track_with_options(id, { encoder: Some(TrackVideoEncoder::Inline(cb)), … })` |
| `PeerConnectionFactory::with_encoded_video_track(id, w, h)` | `create_video_track_with_options(id, { encoder: Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(w, h))), … })` → `LocalVideoTrack::Encoded` |
| `PeerConnectionFactory::encoded_video_builder()` (a.k.a `EncodedVideoBuilder`, `MixedVideoTrack`) | `create_video_track_with_options`, one call per track — raw and pre-encoded mix freely on one factory |
| `CustomVideoEncoder`(public) | `TrackVideoEncoder::{ PreEncoded, Inline }` (one slot mechanism; `InlineEncoderCallback` alias) |
| `Track::push_video_frame(_at)` | `VideoTrack::push_frame(VideoFrame::new(..))` / `push_frame_at(.., capture_time_us)?` |
| `Track::push_video_frame_with_metadata(_at)` | `VideoTrack::push_frame_with_metadata(_at)(frame, user_data)?` |
| `Track::push_pcm(_at)` | `AudioTrack::push_frame(AudioFrame::new(..))` / `push_frame_at(..)` |
| `EncodedVideoTrack::push_encoded_frame(_with_metadata)` | `EncodedVideoTrack::push_frame(_with_metadata)` |
| `Track::on_video_frame` / `on_audio_frame` | `VideoTrack::on_frame` / `AudioTrack::on_frame` |
| `PeerConnectionObserver::on_track` with `(MediaKind, Track)` | `on_track` with one [`RemoteTrack`](Video/Audio) per call |
| `PeerConnectionFactory::create_audio_track_with_local_source(id)` | **kept as a deprecated shim** over `create_audio_track_with_options(id, { source: AudioTrackSource::LocalPush, … })` — deprecated but functional so 0.12 callers keep building |
| FFI `reactor_webrtc_factory_create(_with_adm/_with_adm_apm/_with_custom_video_encoder/_with_openh264)` | FFI `reactor_webrtc_factory_create(const ReactorFactoryOptions*, err, err_cap)` |
| FFI `reactor_webrtc_audio_track_create_with_local_source` | `reactor_webrtc_audio_track_create(factory, id, const ReactorAudioTrackOptions*)` |

### Added in Rust

**Composable factory builder** — process-physical knobs only, everything
else moved to track options:

```rust
let factory = PeerConnectionFactory::builder()
    .with_platform_adm()             // or .with_synthetic_adm() / .with_adm(mode)
    .with_apm(ApmConfig { .. })      // DSP chain, all-off default
    .with_metadata(true)             // factory-wide frame-metadata kill switch
    .with_openh264(&lib_path)        // feature-gated OpenH264 registration
    .build()?;
```

**Per-track video options** — `VideoTrackOptions { encoder, h264_backend, frame_metadata }`:

```rust
// Pre-encoded (push already-encoded bytes at your own pace):
let screen = factory.create_video_track_with_options("scr", {
    let mut o = VideoTrackOptions::default();
    o.encoder = Some(TrackVideoEncoder::PreEncoded(PreEncodedOptions::new(1920, 1080)));
    o.frame_metadata = Some(false);        // drop this track's trailers
    o
})?;

// Inline encoder callback (libwebrtc calls you with every raw frame):
let v = factory.create_video_track_with_options("cam", {
    let mut o = VideoTrackOptions::default();
    o.encoder = Some(TrackVideoEncoder::Inline(Box::new(|raw| encode(raw)?)));
    o.frame_metadata = Some(false);
    o
})?;
```

Any mix of raw / pre-encoded / inline tracks shares one factory; the
positional slot-assignment rule is documented on `create_video_track`.
`H264Backend { VideoToolbox, OpenH264 }` selects the H.264 engine per raw
track (Auto = platform default: VideoToolbox on Apple, OpenH264 elsewhere;
a forced-but-unusable backend fails at track creation).
`VideoTrackOptions::frame_metadata: Option<bool>` overrides the per-track
trailer.

**Per-track audio options** — `AudioTrackOptions { source: Adm|LocalPush, echo_cancellation, noise_suppression, auto_gain_control, high_pass_filter: Option<bool> }`. `LocalPush` is the per-track push pipe (music next to mic).

**Typed media wrappers** — `VideoTrack` / `AudioTrack` / `EncodedVideoTrack`
(local and remote) alongside the untyped `Track` core (everything pushes via
`push_frame` and friends; pushing onto a remote track is an `Err`). `on_track`
delivers `RemoteTrack::{ Video, Audio }`.

**Encoder feedback** — `EncoderFeedback::{ KeyFrameRequest, RateUpdate { bitrate_bps, framerate_fps } }` through
`EncodedVideoTrack::on_encoder_feedback` (pre-encoded) and
`VideoTrack::on_encoder_feedback` (inline). SetRates was a silent
no-op for custom encoders previously — BWE now actually reaches your code.
Answer a `KeyFrameRequest` by pushing an IDR promptly.

**Example upgrades** — `pre_encoded` (IDR-on-demand via feedback), `multi_track`
(all three modes side by side), `openh264_codec` (both H.264 choices in one
factory), `music_mic_simulated` (portable mic+music showcase) and
`music_and_mic` (real device).

### Behavioral notes worth a second look

- **SDP advertisement is now dynamic**: H264/H265 are claimed exactly when a
  custom slot could feed a bitstream (pre-encoded/inline slots present) or a
  real backend exists — plain factories keep the builtin set. Decoder-side
  blacklist got stricter for plain factories (they never silently negotiate a
  codec they can't decode).
- **macOS + registered OpenH264**: explicit registration no longer outranks
  VideoToolbox for raw tracks on Auto; force it per track when you prefer it.
- **Every raw track creation reserves a registry slot**, so a raw track can
  no longer steal another track's pre-encoded slot by side effect.
- **Python stays compatible**: names intact; new in 0.13 are
  `PeerConnectionFactoryBuilder` (also `PeerConnectionFactory.builder()`),
  `create_video_track_with_options` / `create_audio_track_with_options`,
  `on_encoder_feedback` (+ `KeyFrameRequest`, `RateUpdate`,
  `AudioTrackSource`, `H264Backend`, `RawVideoFrameInfo`) and, on wheels built
  with the feature, `builder.with_openh264(lib_path)` /
  `H264Backend.OpenH264`.

### Milestone

[REA-5601](https://linear.app/reactor-team/issue/REA-5601) — full stack under
milestone *v0.13 — composable factory builder + per-track options*.
