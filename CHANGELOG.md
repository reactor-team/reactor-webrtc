# Changelog

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
