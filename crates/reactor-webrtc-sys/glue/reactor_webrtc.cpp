// reactor-webrtc glue — the C++ side of the FFI boundary.
//
// M1: the first real objects backed by our owned libwebrtc.a — the builtin
// codec factories, a real PeerConnectionFactory (threads + media engine), and
// PeerConnections with a callback bridge (PeerConnectionObserver +
// CreateSessionDescriptionObserver forwarded to C function pointers).
//
// The builtin codec factories live in their own gn targets that the `webrtc`
// umbrella normally omits; webrtc-build/patches/0001-* injects them into the
// umbrella's deps so these symbols resolve in libwebrtc.a.
//
// The remaining surface in `src/lib.rs` (tracks, frame I/O, ADM, set-remote-
// description / add-ice-candidate) lands next and will eventually be generated
// rather than hand-written.

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "api/audio/audio_device.h"
#include "api/audio/audio_device_defines.h"
#include "api/audio/audio_processing.h"
#include "api/audio/builtin_audio_processing_builder.h"
#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/audio_options.h"
#include "api/create_peerconnection_factory.h"
#include "api/data_channel_interface.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/frame_transformer_interface.h"
#include "api/jsep.h"
#include "api/make_ref_counted.h"
#include "api/media_stream_interface.h"
#include "api/media_types.h"
#include "api/peer_connection_interface.h"
#include "api/rtc_error.h"
#include "api/rtp_receiver_interface.h"
#include "api/rtp_sender_interface.h"
#include "api/rtp_transceiver_direction.h"
#include "api/rtp_transceiver_interface.h"
#include "api/scoped_refptr.h"
#include "api/set_local_description_observer_interface.h"
#include "api/set_remote_description_observer_interface.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "api/video/video_sink_interface.h"
#include "api/video/video_source_interface.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "media/base/video_broadcaster.h"
#include "modules/audio_device/include/audio_device_default.h"
#include "modules/video_coding/codecs/interface/common_constants.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "pc/video_track_source.h"
#include "rtc_base/copy_on_write_buffer.h"
#include "rtc_base/thread.h"
#include "rtc_base/time_utils.h"
#include "third_party/libyuv/include/libyuv/convert.h"
#include "third_party/libyuv/include/libyuv/convert_argb.h"

// ── C ABI types (must match crates/reactor-webrtc-sys/src/lib.rs) ─────────────

extern "C" {
// PeerConnectionObserver events, forwarded to the safe crate. Any pointer may
// be null (the field is `Option<extern "C" fn>` on the Rust side).
struct ReactorPcCallbacks {
  void* userdata;
  void (*on_signaling_change)(void* userdata, int state);
  void (*on_connection_change)(void* userdata, int state);
  void (*on_ice_gathering_change)(void* userdata, int state);
  void (*on_ice_candidate)(void* userdata, const char* sdp_mid,
                           int sdp_mline_index, const char* candidate);
  void (*on_data_channel)(void* userdata, void* data_channel);
  void (*on_renegotiation_needed)(void* userdata);
  // A remote track was added; `track` is an owned MediaStreamTrack handle the
  // receiver must free with reactor_webrtc_media_stream_track_destroy.
  void (*on_track)(void* userdata, void* track);
};
}  // extern "C"

namespace {

// Opt-in audio path tracing (REACTOR_WEBRTC_AUDIO_DEBUG=1) → stderr. Off by
// default. Used to pinpoint capture (push) vs playout-pump vs sink delivery.
bool audio_debug() {
  static const bool on = std::getenv("REACTOR_WEBRTC_AUDIO_DEBUG") != nullptr;
  return on;
}

// A video source we can push externally-produced frames into. VideoBroadcaster
// fans frames out to the connected encoder sink(s).
class FrameSource : public webrtc::VideoTrackSource {
 public:
  FrameSource() : webrtc::VideoTrackSource(/*remote=*/false) {}
  void PushFrame(const webrtc::VideoFrame& frame) { broadcaster_.OnFrame(frame); }

 protected:
  webrtc::VideoSourceInterface<webrtc::VideoFrame>* source() override {
    return &broadcaster_;
  }

 private:
  webrtc::VideoBroadcaster broadcaster_;
};

// A custom AudioDeviceModule we push PCM into (capture) and which pumps the
// receive path (playout) so remote audio-track sinks fire — no real audio
// hardware. AudioDeviceModuleDefault stubs the ~40 ADM methods; we override the
// capture/playout bits.
class FrameAdm : public webrtc::webrtc_impl::AudioDeviceModuleDefault<
                     webrtc::AudioDeviceModule> {
 public:
  ~FrameAdm() override { StopPlayout(); }

  int32_t RegisterAudioCallback(webrtc::AudioTransport* transport) override {
    std::lock_guard<std::mutex> lock(mutex_);
    transport_ = transport;
    if (audio_debug())
      fprintf(stderr, "[reactor-webrtc] ADM RegisterAudioCallback transport=%p\n",
              static_cast<void*>(transport));
    return 0;
  }
  bool RecordingIsInitialized() const override { return true; }
  bool Recording() const override { return true; }

  // Gate the playout pump (e.g. to stay fully silent in send-only / headless
  // scenarios). Enabled by default.
  void SetPlayoutEnabled(bool enabled) { playout_enabled_.store(enabled); }

  // Playout pump: pulls (and discards) 10ms render blocks so the receive
  // pipeline runs and remote audio-track sinks are invoked.
  int32_t StartPlayout() override {
    if (audio_debug())
      fprintf(stderr, "[reactor-webrtc] ADM StartPlayout (enabled=%d already=%d)\n",
              playout_enabled_.load(), playing_.load());
    if (!playout_enabled_.load()) return 0;
    if (playing_.exchange(true)) return 0;
    play_thread_ = std::thread([this] { PlayoutLoop(); });
    return 0;
  }
  int32_t StopPlayout() override {
    if (!playing_.exchange(false)) return 0;
    if (play_thread_.joinable()) play_thread_.join();
    return 0;
  }
  bool Playing() const override { return playing_.load(); }

  // Deliver interleaved int16 PCM (samples_per_channel frames) to the engine.
  void PushPcm(const int16_t* pcm, size_t samples_per_channel,
               uint32_t sample_rate, size_t channels) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!transport_ || !pcm || channels == 0) return;
    if (audio_debug()) {
      static uint64_t n = 0;
      if (++n % 500 == 1)
        fprintf(stderr, "[reactor-webrtc] ADM PushPcm #%llu (%zu frames %u Hz %zu ch)\n",
                (unsigned long long)n, samples_per_channel, sample_rate, channels);
    }
    uint32_t new_mic_level = 0;
    transport_->RecordedDataIsAvailable(
        pcm, samples_per_channel, sizeof(int16_t) * channels, channels,
        sample_rate, /*totalDelayMS=*/0, /*clockDrift=*/0, /*currentMicLevel=*/0,
        /*keyPressed=*/false, new_mic_level);
  }

 private:
  void PlayoutLoop() {
    const uint32_t rate = 48000;
    const size_t channels = 2;
    const size_t frames = rate / 100;  // 10ms
    std::vector<int16_t> scratch(frames * channels);
    uint64_t pulls = 0, produced = 0;
    while (playing_.load()) {
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (transport_) {
          size_t out = 0;
          int64_t elapsed = 0, ntp = 0;
          transport_->NeedMorePlayData(frames, sizeof(int16_t) * channels,
                                       channels, rate, scratch.data(), out,
                                       &elapsed, &ntp);
          // out is just the requested frame count echoed back; measure the
          // mixed peak to tell real incoming audio from silence.
          int16_t peak = 0;
          for (size_t i = 0; i < out * channels && i < scratch.size(); ++i) {
            int16_t v = scratch[i] < 0 ? -scratch[i] : scratch[i];
            if (v > peak) peak = v;
          }
          if (peak > 0) ++produced;
          if (audio_debug() && ++pulls % 200 == 1)
            fprintf(stderr,
                    "[reactor-webrtc] ADM playout pump: %llu pulls, %llu non-silent, "
                    "last peak=%d (out=%zu)\n",
                    (unsigned long long)pulls, (unsigned long long)produced, peak, out);
        }
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
  }

  std::mutex mutex_;
  webrtc::AudioTransport* transport_ = nullptr;
  std::atomic<bool> playing_{false};
  std::atomic<bool> playout_enabled_{true};
  std::thread play_thread_;
};

// Bridges decoded frames from a (remote) audio track to a C callback. WebRTC
// audio-sink data is interleaved int16 PCM (bits_per_sample == 16).
class AudioFrameSink : public webrtc::AudioTrackSinkInterface {
 public:
  AudioFrameSink(void* userdata,
                 void (*on_audio)(void*, const int16_t*, int, int, int))
      : userdata_(userdata), on_audio_(on_audio) {}
  void OnData(const void* audio_data, int /*bits_per_sample*/, int sample_rate,
              size_t number_of_channels, size_t number_of_frames) override {
    if (audio_debug()) {
      static uint64_t n = 0;
      if (++n % 100 == 1)
        fprintf(stderr, "[reactor-webrtc] audio sink OnData #%llu (%d Hz %zu ch %zu frames)\n",
                (unsigned long long)n, sample_rate, number_of_channels, number_of_frames);
    }
    if (on_audio_)
      on_audio_(userdata_, static_cast<const int16_t*>(audio_data), sample_rate,
                static_cast<int>(number_of_channels),
                static_cast<int>(number_of_frames));
  }

 private:
  void* userdata_;
  void (*on_audio_)(void*, const int16_t*, int, int, int);
};

// Bridges decoded frames from a (remote) video track to a C callback,
// converting to BGRA (width*height*4) on the way out.
class FrameSink : public webrtc::VideoSinkInterface<webrtc::VideoFrame> {
 public:
  FrameSink(void* userdata, void (*on_frame)(void*, const uint8_t*, int, int))
      : userdata_(userdata), on_frame_(on_frame) {}
  void OnFrame(const webrtc::VideoFrame& frame) override {
    if (!on_frame_) return;
    webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 =
        frame.video_frame_buffer()->ToI420();
    if (!i420) return;
    const int w = i420->width(), h = i420->height();
    bgra_.resize(static_cast<size_t>(w) * h * 4);
    // libyuv "ARGB" is B,G,R,A in memory == our BGRA byte order.
    libyuv::I420ToARGB(i420->DataY(), i420->StrideY(), i420->DataU(),
                       i420->StrideU(), i420->DataV(), i420->StrideV(),
                       bgra_.data(), w * 4, w, h);
    on_frame_(userdata_, bgra_.data(), w, h);
  }

 private:
  void* userdata_;
  void (*on_frame_)(void*, const uint8_t*, int, int);
  std::vector<uint8_t> bgra_;
};

// A track handle (the `MediaStreamTrack` in the Rust API). For a local track
// `source` is set and frames can be pushed; for a remote track (from OnTrack)
// `source` is null and `sink` is attached to receive frames.
struct ReactorMediaStreamTrack {
  webrtc::scoped_refptr<FrameSource> source;
  webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track;
  std::unique_ptr<FrameSink> sink;
  std::unique_ptr<AudioFrameSink> audio_sink;
};

// Bridges data-channel events to C callbacks.
class ReactorDcObserver : public webrtc::DataChannelObserver {
 public:
  ReactorDcObserver(webrtc::DataChannelInterface* channel, void* userdata,
                    void (*on_message)(void*, const uint8_t*, size_t, int),
                    void (*on_open)(void*), void (*on_close)(void*))
      : channel_(channel),
        userdata_(userdata),
        on_message_(on_message),
        on_open_(on_open),
        on_close_(on_close) {}

  void OnStateChange() override {
    switch (channel_->state()) {
      case webrtc::DataChannelInterface::kOpen:
        if (on_open_) on_open_(userdata_);
        break;
      case webrtc::DataChannelInterface::kClosed:
        if (on_close_) on_close_(userdata_);
        break;
      default:
        break;
    }
  }
  void OnMessage(const webrtc::DataBuffer& buffer) override {
    if (on_message_)
      on_message_(userdata_, buffer.data.cdata(), buffer.data.size(),
                  buffer.binary ? 1 : 0);
  }

 private:
  webrtc::DataChannelInterface* channel_;  // not owned
  void* userdata_;
  void (*on_message_)(void*, const uint8_t*, size_t, int);
  void (*on_open_)(void*);
  void (*on_close_)(void*);
};

// A data-channel handle (the `DataChannel` in the Rust API): the channel plus
// its (optional) registered observer.
struct ReactorDataChannel {
  webrtc::scoped_refptr<webrtc::DataChannelInterface> channel;
  std::unique_ptr<ReactorDcObserver> observer;
};

// A transceiver handle (the `RtpTransceiver` in the Rust API).
struct ReactorTransceiver {
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> tc;
};

// Owns the three WebRTC threads alongside the factory. The threads must outlive
// the factory, so declare `factory` last: members are destroyed in reverse
// declaration order, releasing the factory before the threads stop.
struct ReactorFactory {
  std::unique_ptr<webrtc::Thread> network_thread;
  std::unique_ptr<webrtc::Thread> worker_thread;
  std::unique_ptr<webrtc::Thread> signaling_thread;
  webrtc::scoped_refptr<FrameAdm> adm;
  webrtc::scoped_refptr<webrtc::PeerConnectionFactoryInterface> factory;
};

// Bridges PeerConnectionObserver callbacks to the C function pointers.
class ReactorPcObserver : public webrtc::PeerConnectionObserver {
 public:
  explicit ReactorPcObserver(const ReactorPcCallbacks& cb) : cb_(cb) {}

  void OnSignalingChange(
      webrtc::PeerConnectionInterface::SignalingState new_state) override {
    if (cb_.on_signaling_change)
      cb_.on_signaling_change(cb_.userdata, static_cast<int>(new_state));
  }
  void OnConnectionChange(
      webrtc::PeerConnectionInterface::PeerConnectionState new_state) override {
    if (cb_.on_connection_change)
      cb_.on_connection_change(cb_.userdata, static_cast<int>(new_state));
  }
  void OnIceGatheringChange(
      webrtc::PeerConnectionInterface::IceGatheringState new_state) override {
    if (cb_.on_ice_gathering_change)
      cb_.on_ice_gathering_change(cb_.userdata, static_cast<int>(new_state));
  }
  void OnIceCandidate(const webrtc::IceCandidate* candidate) override {
    if (!cb_.on_ice_candidate || !candidate) return;
    std::string sdp;
    candidate->ToString(&sdp);
    cb_.on_ice_candidate(cb_.userdata, candidate->sdp_mid().c_str(),
                         candidate->sdp_mline_index(), sdp.c_str());
  }
  void OnDataChannel(
      webrtc::scoped_refptr<webrtc::DataChannelInterface> dc) override {
    if (!cb_.on_data_channel) return;
    cb_.on_data_channel(cb_.userdata,
                        new ReactorDataChannel{std::move(dc), nullptr});
  }
  // Forward both the legacy and the spec-compliant negotiation signals.
  void OnRenegotiationNeeded() override { fire_renegotiation(); }
  void OnNegotiationNeededEvent(uint32_t /*event_id*/) override {
    fire_renegotiation();
  }
  void OnTrack(
      webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> transceiver)
      override {
    if (!cb_.on_track || !transceiver) return;
    webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver =
        transceiver->receiver();
    if (!receiver) return;
    webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track =
        receiver->track();
    if (!track) return;
    auto* handle = new ReactorMediaStreamTrack();
    handle->track = std::move(track);
    cb_.on_track(cb_.userdata, handle);
  }

 private:
  void fire_renegotiation() {
    if (cb_.on_renegotiation_needed) cb_.on_renegotiation_needed(cb_.userdata);
  }
  ReactorPcCallbacks cb_;
};

// Holds the observer alongside the peer connection. The PC holds a raw pointer
// to the observer, so the observer must outlive it: declare `pc` last so it is
// released first.
struct ReactorPeerConnection {
  std::unique_ptr<ReactorPcObserver> observer;
  webrtc::scoped_refptr<webrtc::PeerConnectionInterface> pc;
};

// Forwards CreateOffer/CreateAnswer results to C callbacks.
class CreateSdpObserver : public webrtc::CreateSessionDescriptionObserver {
 public:
  CreateSdpObserver(void* userdata,
                    void (*on_success)(void*, const char*, const char*),
                    void (*on_error)(void*, const char*))
      : userdata_(userdata), on_success_(on_success), on_error_(on_error) {}

  void OnSuccess(webrtc::SessionDescriptionInterface* desc) override {
    std::string sdp;
    desc->ToString(&sdp);
    if (on_success_) on_success_(userdata_, desc->type().c_str(), sdp.c_str());
    // OnSuccess transfers ownership of `desc` to us; we don't keep it here.
    delete desc;
  }
  void OnFailure(webrtc::RTCError error) override {
    if (on_error_) on_error_(userdata_, error.message());
  }

 private:
  void* userdata_;
  void (*on_success_)(void*, const char*, const char*);
  void (*on_error_)(void*, const char*);
};

// Forwards SetLocalDescription completion to a C callback (null error = ok).
class SetLocalDescObserver
    : public webrtc::SetLocalDescriptionObserverInterface {
 public:
  SetLocalDescObserver(void* userdata, void (*on_complete)(void*, const char*))
      : userdata_(userdata), on_complete_(on_complete) {}
  void OnSetLocalDescriptionComplete(webrtc::RTCError error) override {
    if (on_complete_)
      on_complete_(userdata_, error.ok() ? nullptr : error.message());
  }

 private:
  void* userdata_;
  void (*on_complete_)(void*, const char*);
};

// Forwards SetRemoteDescription completion to a C callback (null error = ok).
class SetRemoteDescObserver
    : public webrtc::SetRemoteDescriptionObserverInterface {
 public:
  SetRemoteDescObserver(void* userdata, void (*on_complete)(void*, const char*))
      : userdata_(userdata), on_complete_(on_complete) {}
  void OnSetRemoteDescriptionComplete(webrtc::RTCError error) override {
    if (on_complete_)
      on_complete_(userdata_, error.ok() ? nullptr : error.message());
  }

 private:
  void* userdata_;
  void (*on_complete_)(void*, const char*);
};

// Lenient ICE-server extraction: pull quoted stun:/turn[s]: URLs out of the
// config JSON without a JSON dependency. TODO(M1): replace with the structured
// config the safe crate will build.
void parse_ice_servers(const char* config_json,
                       webrtc::PeerConnectionInterface::RTCConfiguration& cfg) {
  if (!config_json) return;
  const std::string s(config_json);
  size_t start = 0;
  while ((start = s.find('"', start)) != std::string::npos) {
    const size_t end = s.find('"', start + 1);
    if (end == std::string::npos) break;
    const std::string tok = s.substr(start + 1, end - start - 1);
    if (tok.rfind("stun:", 0) == 0 || tok.rfind("stuns:", 0) == 0 ||
        tok.rfind("turn:", 0) == 0 || tok.rfind("turns:", 0) == 0) {
      webrtc::PeerConnectionInterface::IceServer srv;
      srv.urls.push_back(tok);
      cfg.servers.push_back(std::move(srv));
    }
    start = end + 1;
  }
}

}  // namespace

extern "C" {

// ABI version of this native build. The safe crate asserts compatibility.
unsigned int reactor_webrtc_abi_version() { return 1; }

// Link/run self-test: build the builtin audio + video encoder factories and
// enumerate the codecs they support. Writes a comma-separated, NUL-terminated
// list of codec names into `out` (truncated to `cap`) and returns the total
// count. A non-zero return means real libwebrtc code linked and executed.
int reactor_webrtc_selftest(char* out, int cap) {
  std::string names;
  int count = 0;

  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> audio =
      webrtc::CreateBuiltinAudioEncoderFactory();
  for (const webrtc::AudioCodecSpec& spec : audio->GetSupportedEncoders()) {
    if (!names.empty()) names += ",";
    names += spec.format.name;
    ++count;
  }

  std::unique_ptr<webrtc::VideoEncoderFactory> video =
      webrtc::CreateBuiltinVideoEncoderFactory();
  for (const webrtc::SdpVideoFormat& fmt : video->GetSupportedFormats()) {
    if (!names.empty()) names += ",";
    names += fmt.name;
    ++count;
  }

  if (out && cap > 0) {
    std::strncpy(out, names.c_str(), static_cast<size_t>(cap) - 1);
    out[cap - 1] = '\0';
  }
  return count;
}

// Create a PeerConnectionFactory: start the network/worker/signaling threads
// and wire the builtin audio+video codec factories.
//
// `use_platform_adm`: 0 → our synthetic FrameAdm (push PCM, no hardware);
// nonzero → pass a null ADM so the media engine creates the platform default
// ADM (real mic/speaker, e.g. CoreAudio on macOS). Returns an opaque
// ReactorFactory* (the `PeerConnectionFactory` handle), or nullptr.
void* reactor_webrtc_factory_create_with_adm(int use_platform_adm) {
  auto f = std::make_unique<ReactorFactory>();

  f->network_thread = webrtc::Thread::CreateWithSocketServer();
  f->worker_thread = webrtc::Thread::Create();
  f->signaling_thread = webrtc::Thread::Create();
  if (!f->network_thread->Start() || !f->worker_thread->Start() ||
      !f->signaling_thread->Start()) {
    return nullptr;
  }

  // Synthetic ADM unless the platform default is requested (null → engine
  // creates the real device ADM internally).
  webrtc::scoped_refptr<webrtc::AudioDeviceModule> adm;
  webrtc::scoped_refptr<webrtc::AudioProcessing> apm;
  if (use_platform_adm) {
    // Real mic capture → enable the standard capture-processing chain:
    // AEC3 + noise suppression + AGC + high-pass filter. (Bandwidth estimation
    // / GoogCC is always compiled in and active for media.) The synthetic ADM
    // path stays passthrough (bit-exact PCM push, e.g. server forwarding).
    webrtc::AudioProcessing::Config apm_config;
    apm_config.echo_canceller.enabled = true;
    apm_config.noise_suppression.enabled = true;
    apm_config.noise_suppression.level =
        webrtc::AudioProcessing::Config::NoiseSuppression::kHigh;
    apm_config.gain_controller1.enabled = true;
    apm_config.high_pass_filter.enabled = true;
    apm = webrtc::BuiltinAudioProcessingBuilder(apm_config)
              .Build(webrtc::CreateEnvironment());
  } else {
    f->adm = webrtc::make_ref_counted<FrameAdm>();
    adm = f->adm;
  }

  f->factory = webrtc::CreatePeerConnectionFactory(
      f->network_thread.get(), f->worker_thread.get(),
      f->signaling_thread.get(), adm,
      webrtc::CreateBuiltinAudioEncoderFactory(),
      webrtc::CreateBuiltinAudioDecoderFactory(),
      webrtc::CreateBuiltinVideoEncoderFactory(),
      webrtc::CreateBuiltinVideoDecoderFactory(),
      /*audio_mixer=*/nullptr, /*audio_processing=*/apm);
  if (!f->factory) {
    return nullptr;  // threads stopped by ReactorFactory's destructor
  }
  return f.release();
}

// Create a factory with the synthetic (push-able) ADM.
void* reactor_webrtc_factory_create() {
  return reactor_webrtc_factory_create_with_adm(0);
}

// Enable/disable the synthetic ADM's playout pump (no-op for the platform ADM).
void reactor_webrtc_factory_set_adm_playout_enabled(void* factory, int enabled) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (rf && rf->adm) rf->adm->SetPlayoutEnabled(enabled != 0);
}

// Destroy a factory created by reactor_webrtc_factory_create (releases the
// factory, then stops + joins the threads).
void reactor_webrtc_factory_destroy(void* factory) {
  delete reinterpret_cast<ReactorFactory*>(factory);
}

// Create a PeerConnection on `factory`. `config_json` may be null/empty;
// recognized ICE-server URLs are applied. `callbacks` may be null. Returns an
// opaque ReactorPeerConnection* (the `PeerConnection` handle), or nullptr.
void* reactor_webrtc_peer_connection_create(void* factory,
                                            const char* config_json,
                                            const ReactorPcCallbacks* callbacks) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) return nullptr;

  auto rpc = std::make_unique<ReactorPeerConnection>();
  ReactorPcCallbacks cb{};
  if (callbacks) cb = *callbacks;
  rpc->observer = std::make_unique<ReactorPcObserver>(cb);

  webrtc::PeerConnectionInterface::RTCConfiguration config;
  config.sdp_semantics = webrtc::SdpSemantics::kUnifiedPlan;
  parse_ice_servers(config_json, config);

  webrtc::PeerConnectionDependencies deps(rpc->observer.get());
  auto result =
      rf->factory->CreatePeerConnectionOrError(config, std::move(deps));
  if (!result.ok()) return nullptr;
  rpc->pc = result.MoveValue();
  return rpc.release();
}

// Close + destroy a PeerConnection.
void reactor_webrtc_peer_connection_destroy(void* pc) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (rpc && rpc->pc) rpc->pc->Close();
  delete rpc;
}

// Create an offer. The result is delivered asynchronously on the signaling
// thread to exactly one of the callbacks.
void reactor_webrtc_peer_connection_create_offer(
    void* pc, void* userdata,
    void (*on_success)(void* userdata, const char* type, const char* sdp),
    void (*on_error)(void* userdata, const char* message)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_error) on_error(userdata, "no peer connection");
    return;
  }
  auto observer =
      webrtc::make_ref_counted<CreateSdpObserver>(userdata, on_success, on_error);
  webrtc::PeerConnectionInterface::RTCOfferAnswerOptions options;
  rpc->pc->CreateOffer(observer.get(), options);
}

// Create an answer (current signaling state must have a remote offer). Result
// delivered like create_offer.
void reactor_webrtc_peer_connection_create_answer(
    void* pc, void* userdata,
    void (*on_success)(void* userdata, const char* type, const char* sdp),
    void (*on_error)(void* userdata, const char* message)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_error) on_error(userdata, "no peer connection");
    return;
  }
  auto observer =
      webrtc::make_ref_counted<CreateSdpObserver>(userdata, on_success, on_error);
  webrtc::PeerConnectionInterface::RTCOfferAnswerOptions options;
  rpc->pc->CreateAnswer(observer.get(), options);
}

// Parse (type, sdp) and apply it as the local description. `on_complete` fires
// once with a null error on success.
void reactor_webrtc_peer_connection_set_local_description(
    void* pc, const char* type, const char* sdp, void* userdata,
    void (*on_complete)(void* userdata, const char* error)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_complete) on_complete(userdata, "no peer connection");
    return;
  }
  const std::optional<webrtc::SdpType> t =
      webrtc::SdpTypeFromString(type ? type : "");
  if (!t) {
    if (on_complete) on_complete(userdata, "invalid sdp type");
    return;
  }
  std::unique_ptr<webrtc::SessionDescriptionInterface> desc =
      webrtc::CreateSessionDescription(*t, sdp ? sdp : "");
  if (!desc) {
    if (on_complete) on_complete(userdata, "failed to parse sdp");
    return;
  }
  rpc->pc->SetLocalDescription(
      std::move(desc),
      webrtc::make_ref_counted<SetLocalDescObserver>(userdata, on_complete));
}

// Parse (type, sdp) and apply it as the remote description.
void reactor_webrtc_peer_connection_set_remote_description(
    void* pc, const char* type, const char* sdp, void* userdata,
    void (*on_complete)(void* userdata, const char* error)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_complete) on_complete(userdata, "no peer connection");
    return;
  }
  const std::optional<webrtc::SdpType> t =
      webrtc::SdpTypeFromString(type ? type : "");
  if (!t) {
    if (on_complete) on_complete(userdata, "invalid sdp type");
    return;
  }
  std::unique_ptr<webrtc::SessionDescriptionInterface> desc =
      webrtc::CreateSessionDescription(*t, sdp ? sdp : "");
  if (!desc) {
    if (on_complete) on_complete(userdata, "failed to parse sdp");
    return;
  }
  rpc->pc->SetRemoteDescription(
      std::move(desc),
      webrtc::make_ref_counted<SetRemoteDescObserver>(userdata, on_complete));
}

// Add a remote ICE candidate (from the peer's OnIceCandidate). `on_complete`
// fires once with a null error on success.
void reactor_webrtc_peer_connection_add_ice_candidate(
    void* pc, const char* sdp_mid, int sdp_mline_index, const char* candidate,
    void* userdata, void (*on_complete)(void* userdata, const char* error)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_complete) on_complete(userdata, "no peer connection");
    return;
  }
  webrtc::SdpParseError err;
  std::unique_ptr<webrtc::IceCandidate> cand(webrtc::CreateIceCandidate(
      sdp_mid ? sdp_mid : "", sdp_mline_index,
      std::string(candidate ? candidate : ""), &err));
  if (!cand) {
    if (on_complete) on_complete(userdata, err.description.c_str());
    return;
  }
  rpc->pc->AddIceCandidate(
      std::move(cand), [userdata, on_complete](webrtc::RTCError error) {
        if (on_complete)
          on_complete(userdata, error.ok() ? nullptr : error.message());
      });
}

// Create a (negotiated-by-SDP) data channel. Returns an opaque DataChannel
// handle (a heap scoped_refptr) or nullptr.
void* reactor_webrtc_peer_connection_create_data_channel(void* pc,
                                                         const char* label) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) return nullptr;
  webrtc::DataChannelInit init;
  auto result = rpc->pc->CreateDataChannelOrError(label ? label : "", &init);
  if (!result.ok()) return nullptr;
  return new ReactorDataChannel{result.MoveValue(), nullptr};
}

// Send bytes over a data channel. Returns 1 on success, 0 on failure.
int reactor_webrtc_data_channel_send(void* data_channel, const uint8_t* data,
                                     size_t len, int binary) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel) return 0;
  webrtc::CopyOnWriteBuffer buffer(data, len);
  return h->channel->Send(webrtc::DataBuffer(buffer, binary != 0)) ? 1 : 0;
}

// Register callbacks for a data channel. `on_message(userdata, data, len,
// binary)` fires per message; `on_open`/`on_close` on state transitions. Any
// pointer may be null. Replaces a previously registered observer.
void reactor_webrtc_data_channel_register_observer(
    void* data_channel, void* userdata,
    void (*on_message)(void*, const uint8_t*, size_t, int),
    void (*on_open)(void*), void (*on_close)(void*)) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel) return;
  if (h->observer) h->channel->UnregisterObserver();
  h->observer = std::make_unique<ReactorDcObserver>(
      h->channel.get(), userdata, on_message, on_open, on_close);
  h->channel->RegisterObserver(h->observer.get());
}

// Destroy a DataChannel handle (unregisters its observer, releases the channel).
void reactor_webrtc_data_channel_destroy(void* data_channel) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (h && h->channel && h->observer) h->channel->UnregisterObserver();
  delete h;
}

// ── Video tracks ──────────────────────────────────────────────────────────────

// Create a local video track backed by a push-able source. Returns an opaque
// MediaStreamTrack handle (free with reactor_webrtc_media_stream_track_destroy)
// or nullptr.
void* reactor_webrtc_video_track_create(void* factory, const char* id) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) return nullptr;
  auto handle = std::make_unique<ReactorMediaStreamTrack>();
  handle->source = webrtc::make_ref_counted<FrameSource>();
  handle->track = rf->factory->CreateVideoTrack(handle->source, id ? id : "");
  if (!handle->track) return nullptr;
  return handle.release();
}

// Push a BGRA frame (width*height*4 bytes) into a local video track's source.
// Converted to I420 and timestamped here.
void reactor_webrtc_video_track_push_frame(void* track, const uint8_t* bgra,
                                           int width, int height) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->source || !bgra || width <= 0 || height <= 0) return;
  webrtc::scoped_refptr<webrtc::I420Buffer> buffer =
      webrtc::I420Buffer::Create(width, height);
  // libyuv "ARGB" is B,G,R,A in memory == our BGRA byte order.
  libyuv::ARGBToI420(bgra, width * 4, buffer->MutableDataY(), buffer->StrideY(),
                     buffer->MutableDataU(), buffer->StrideU(),
                     buffer->MutableDataV(), buffer->StrideV(), width, height);
  webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
                                 .set_video_frame_buffer(buffer)
                                 .set_timestamp_us(webrtc::TimeMicros())
                                 .build();
  h->source->PushFrame(frame);
}

// Add a (local) audio or video track to the peer connection, creating a
// sendrecv transceiver. Returns 1 on success, 0 on failure.
int reactor_webrtc_peer_connection_add_track(void* pc, void* track) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!rpc || !rpc->pc || !h || !h->track) return 0;
  return rpc->pc->AddTrack(h->track, {"reactor-stream"}).ok() ? 1 : 0;
}

// Create a local audio track. Its samples come from the factory's ADM (push
// PCM with reactor_webrtc_factory_push_audio_frame). Returns an owned
// MediaStreamTrack handle or nullptr.
void* reactor_webrtc_audio_track_create(void* factory, const char* id) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) return nullptr;
  webrtc::scoped_refptr<webrtc::AudioSourceInterface> source =
      rf->factory->CreateAudioSource(webrtc::AudioOptions());
  auto handle = std::make_unique<ReactorMediaStreamTrack>();
  handle->track = rf->factory->CreateAudioTrack(id ? id : "", source.get());
  if (!handle->track) return nullptr;
  return handle.release();
}

// Deliver interleaved int16 PCM to the factory's ADM (shared by all local audio
// tracks). `samples_per_channel` is the frame count (e.g. 480 for 10ms@48kHz).
void reactor_webrtc_factory_push_audio_frame(void* factory, const int16_t* pcm,
                                             int samples_per_channel,
                                             int sample_rate, int channels) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->adm || samples_per_channel <= 0 || channels <= 0) return;
  rf->adm->PushPcm(pcm, static_cast<size_t>(samples_per_channel),
                   static_cast<uint32_t>(sample_rate),
                   static_cast<size_t>(channels));
}

// Attach a frame sink to a (received) audio track. `on_audio(userdata,
// sample_rate, channels, frames)` fires per 10ms block until destroyed.
void reactor_webrtc_audio_track_add_sink(
    void* track, void* userdata,
    void (*on_audio)(void*, const int16_t*, int, int, int)) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track) {
    if (audio_debug()) fprintf(stderr, "[reactor-webrtc] add_audio_sink: null track\n");
    return;
  }
  if (h->track->kind() != "audio") {
    if (audio_debug())
      fprintf(stderr, "[reactor-webrtc] add_audio_sink: track kind=%s (not audio) — skipped\n",
              h->track->kind().c_str());
    return;
  }
  h->audio_sink = std::make_unique<AudioFrameSink>(userdata, on_audio);
  auto* at = static_cast<webrtc::AudioTrackInterface*>(h->track.get());
  at->AddSink(h->audio_sink.get());
  if (audio_debug())
    fprintf(stderr,
            "[reactor-webrtc] add_audio_sink: AddSink OK (track=%p enabled=%d state=%d)\n",
            static_cast<void*>(at), at->enabled(), static_cast<int>(at->state()));
}

// Attach a frame sink to a (video) track. `on_frame(userdata, width, height)`
// fires per decoded frame. The sink lives until the track handle is destroyed.
void reactor_webrtc_video_track_add_sink(
    void* track, void* userdata,
    void (*on_frame)(void*, const uint8_t*, int, int)) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track || h->track->kind() != "video") return;
  h->sink = std::make_unique<FrameSink>(userdata, on_frame);
  static_cast<webrtc::VideoTrackInterface*>(h->track.get())
      ->AddOrUpdateSink(h->sink.get(), webrtc::VideoSinkWants());
}

// Kind of a track handle: 0 = audio, 1 = video, -1 = unknown.
int reactor_webrtc_media_stream_track_kind(void* track) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track) return -1;
  const std::string kind = h->track->kind();
  if (kind == "audio") return 0;
  if (kind == "video") return 1;
  return -1;
}

// ── Transceivers ──────────────────────────────────────────────────────────────

// Add a transceiver of `media_kind` (0=audio, 1=video) with `direction`
// (0=sendrecv, 1=sendonly, 2=recvonly, 3=inactive). Returns an opaque
// RtpTransceiver handle (free with reactor_webrtc_rtp_transceiver_destroy).
void* reactor_webrtc_peer_connection_add_transceiver(void* pc, int media_kind,
                                                     int direction) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) return nullptr;
  webrtc::MediaType mt =
      media_kind == 0 ? webrtc::MediaType::AUDIO : webrtc::MediaType::VIDEO;
  webrtc::RtpTransceiverInit init;
  init.direction = static_cast<webrtc::RtpTransceiverDirection>(direction);
  auto result = rpc->pc->AddTransceiver(mt, init);
  if (!result.ok()) return nullptr;
  return new ReactorTransceiver{result.MoveValue()};
}

// Number of transceivers on the peer connection (post-negotiation this
// includes ones auto-created from the remote description).
int reactor_webrtc_peer_connection_transceiver_count(void* pc) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) return 0;
  return static_cast<int>(rpc->pc->GetTransceivers().size());
}

// Return a new owned handle to the transceiver at `index`
// (free with reactor_webrtc_rtp_transceiver_destroy), or null if out of range.
void* reactor_webrtc_peer_connection_get_transceiver(void* pc, int index) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc || index < 0) return nullptr;
  auto tcs = rpc->pc->GetTransceivers();
  if (static_cast<size_t>(index) >= tcs.size()) return nullptr;
  return new ReactorTransceiver{tcs[static_cast<size_t>(index)]};
}

// Media kind of a transceiver: 0 = audio, 1 = video, -1 = unknown.
int reactor_webrtc_rtp_transceiver_media_kind(void* transceiver) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return -1;
  switch (h->tc->media_type()) {
    case webrtc::MediaType::AUDIO:
      return 0;
    case webrtc::MediaType::VIDEO:
      return 1;
    default:
      return -1;
  }
}

// Write the transceiver's mid into `out` (NUL-terminated, capped at `cap`).
// Returns the mid length, or -1 if there is no mid yet (before SLD).
int reactor_webrtc_rtp_transceiver_mid(void* transceiver, char* out, int cap) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return -1;
  const std::optional<std::string> mid = h->tc->mid();
  if (!mid) return -1;
  if (out && cap > 0) {
    std::strncpy(out, mid->c_str(), static_cast<size_t>(cap) - 1);
    out[cap - 1] = '\0';
  }
  return static_cast<int>(mid->size());
}

// Attach (or clear, with null) a local track on the transceiver's sender.
// Returns 1 on success, 0 on failure.
int reactor_webrtc_rtp_transceiver_set_track(void* transceiver, void* track) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return 0;
  auto* t = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  webrtc::MediaStreamTrackInterface* raw = (t && t->track) ? t->track.get() : nullptr;
  return h->tc->sender()->SetTrack(raw) ? 1 : 0;
}

// Destroy a transceiver handle (releases our reference).
void reactor_webrtc_rtp_transceiver_destroy(void* transceiver) {
  delete reinterpret_cast<ReactorTransceiver*>(transceiver);
}

// ── Encoded-frame transform (codec bypass / forward) ─────────────────────────
//
// WebRTC's Insertable Streams / Encoded Transform: a FrameTransformerInterface
// attached to a sender (SetFrameTransformer via
// SetEncoderToPacketizerFrameTransformer) sees each *encoded* frame after the
// encoder and before packetization; on a receiver
// (SetDepacketizerToDecoderFrameTransformer) it sees each encoded frame after
// depacketization and before the decoder. The bindings can read the encoded
// payload (to forward it elsewhere), optionally replace it, and choose whether
// to emit it downstream — dropping it on the receive side bypasses the decoder.

extern "C" {
// Encoded frame handed to the callback. `data`/`mime_type` are valid only for
// the duration of the call. `frame` is an opaque handle for set_data.
struct ReactorEncodedFrame {
  int direction;        // 0 = send (egress), 1 = receive (ingress)
  int is_audio;         // 1 = audio, 0 = video
  int is_key_frame;     // video only (0 for audio)
  uint8_t payload_type;
  uint32_t ssrc;
  uint32_t timestamp;
  const uint8_t* data;  // encoded payload
  size_t data_len;
  const char* mime_type;  // e.g. "video/VP8", "audio/opus"
  void* frame;          // opaque -> reactor_webrtc_encoded_frame_set_data
};
// Return 0 to emit the frame downstream (after any set_data), non-zero to drop
// it (receive side: bypasses the decoder; send side: nothing is sent).
typedef int (*reactor_webrtc_encoded_frame_cb)(void* userdata,
                                               const ReactorEncodedFrame* frame);
// Called once when the transformer is finally destroyed (all refs dropped) so
// the binding can free `userdata`. May be null.
typedef void (*reactor_webrtc_userdata_free)(void* userdata);
}

// ── Custom video encoder (encoder bypass) ────────────────────────────────────
//
// Raw I420 frame delivered to Rust for encoding. Planes are valid only for
// the duration of the Encode() call — copy if encoding is asynchronous.
//
// `codec` mirrors webrtc::VideoCodecType: VP8=1, VP9=2, AV1=3, H264=4, H265=5.
// It tells the callback which codec was negotiated for this session so the
// application can produce the right bitstream.
extern "C" {
struct ReactorRawVideoFrame {
  const uint8_t* y;
  int            y_stride;
  const uint8_t* u;
  int            u_stride;
  const uint8_t* v;
  int            v_stride;
  uint32_t       width;
  uint32_t       height;
  uint32_t       rtp_timestamp;
  int            request_key_frame; // 1 = IDR/keyframe requested
  uint32_t       codec;             // VideoCodecType value (VP8=1 … H265=5)
  uint64_t       encoder_id;       // unique per ReactorVideoEncoder instance
};
// Filled by the Rust callback to deliver an encoded frame back.
// Set data=nullptr (or return non-zero) to drop the frame (nothing is sent).
// `free_data` is called after the encoded bytes are copied; allows the caller
// to free the buffer. May be null if the buffer has static/frame lifetime.
struct ReactorEncodedVideoOutput {
  const uint8_t* data;
  size_t         len;
  int            is_key_frame;
  uint32_t       width;         // 0 = inherit from raw frame
  uint32_t       height;        // 0 = inherit from raw frame
  uint32_t       rtp_timestamp; // 0 = inherit from raw frame
  void           (*free_data)(const uint8_t* data, size_t len); // may be null
};
// Return 0 to forward the encoded frame, non-zero to drop it.
typedef int (*reactor_video_encode_cb)(void*                      userdata,
                                       const ReactorRawVideoFrame* raw,
                                       ReactorEncodedVideoOutput*  out);
// Optional: called by the factory before creating each VideoEncoder instance.
// Return non-zero to delegate to the builtin VP8/VP9/AV1 encoder instead of
// the custom one. May be null (always use custom). `encoder_id` matches the
// value stamped on every subsequent ReactorRawVideoFrame from that encoder.
typedef int (*reactor_use_builtin_cb)(void* userdata, uint64_t encoder_id);
}

struct ReactorEncoderState {
  reactor_video_encode_cb       cb;
  void*                          userdata;
  reactor_webrtc_userdata_free   free_ud;
  reactor_use_builtin_cb         use_builtin; // null = always use custom

  ReactorEncoderState(reactor_video_encode_cb c, void* u, reactor_webrtc_userdata_free f,
                      reactor_use_builtin_cb ub = nullptr)
      : cb(c), userdata(u), free_ud(f), use_builtin(ub) {}
  ~ReactorEncoderState() { if (free_ud) free_ud(userdata); }
  // Disable copy+move: free_ud would fire twice (once on the copy, once on
  // the original) which would double-free `userdata`.
  ReactorEncoderState(const ReactorEncoderState&) = delete;
  ReactorEncoderState(ReactorEncoderState&&)      = delete;
};

class ReactorVideoEncoder : public webrtc::VideoEncoder {
  std::shared_ptr<ReactorEncoderState> state_;
  webrtc::EncodedImageCallback*        callback_   = nullptr;
  uint32_t                             width_      = 0;
  uint32_t                             height_     = 0;
  webrtc::VideoCodecType               codec_type_ = webrtc::kVideoCodecGeneric;
  uint64_t                             id_         = 0;

 public:
  explicit ReactorVideoEncoder(std::shared_ptr<ReactorEncoderState> state,
                               uint64_t id = 0)
      : state_(std::move(state)), id_(id) {}

  int InitEncode(const webrtc::VideoCodec* settings,
                 const Settings&) override {
    if (settings) {
      width_      = static_cast<uint32_t>(settings->width);
      height_     = static_cast<uint32_t>(settings->height);
      codec_type_ = settings->codecType;
    }
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t RegisterEncodeCompleteCallback(
      webrtc::EncodedImageCallback* cb) override {
    callback_ = cb;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Release() override {
    callback_ = nullptr;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Encode(
      const webrtc::VideoFrame&                  frame,
      const std::vector<webrtc::VideoFrameType>* frame_types) override {
    // Convert to I420 (handles any input format including NV12).
    webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 =
        frame.video_frame_buffer()->ToI420();

    bool want_key = false;
    if (frame_types) {
      for (auto ft : *frame_types)
        if (ft == webrtc::VideoFrameType::kVideoFrameKey) want_key = true;
    }

    ReactorRawVideoFrame raw{};
    raw.y                 = i420->DataY();
    raw.y_stride          = i420->StrideY();
    raw.u                 = i420->DataU();
    raw.u_stride          = i420->StrideU();
    raw.v                 = i420->DataV();
    raw.v_stride          = i420->StrideV();
    raw.width             = static_cast<uint32_t>(i420->width());
    raw.height            = static_cast<uint32_t>(i420->height());
    raw.rtp_timestamp     = frame.rtp_timestamp();
    raw.request_key_frame = want_key ? 1 : 0;
    raw.codec             = static_cast<uint32_t>(codec_type_);
    raw.encoder_id        = id_;

    ReactorEncodedVideoOutput out{};
    int drop = state_->cb(state_->userdata, &raw, &out);
    if (drop || !out.data || out.len == 0 || !callback_) {
      return WEBRTC_VIDEO_CODEC_OK;
    }

    webrtc::EncodedImage img;
    img.SetEncodedData(webrtc::EncodedImageBuffer::Create(out.data, out.len));
    img.SetFrameType(out.is_key_frame ? webrtc::VideoFrameType::kVideoFrameKey
                                       : webrtc::VideoFrameType::kVideoFrameDelta);
    img._encodedWidth  = out.width  ? out.width  : raw.width;
    img._encodedHeight = out.height ? out.height : raw.height;
    img.SetRtpTimestamp(out.rtp_timestamp ? out.rtp_timestamp : raw.rtp_timestamp);

    // Build codec-specific metadata that the RTP packetizer needs.
    webrtc::CodecSpecificInfo info;
    info.codecType = codec_type_;
    switch (codec_type_) {
      case webrtc::kVideoCodecH264:
        info.codecSpecific.H264.packetization_mode =
            webrtc::H264PacketizationMode::NonInterleaved;
        info.codecSpecific.H264.temporal_idx  = webrtc::kNoTemporalIdx;
        info.codecSpecific.H264.idr_frame     = (out.is_key_frame != 0);
        info.codecSpecific.H264.base_layer_sync = false;
        break;
      case webrtc::kVideoCodecVP8:
        // keyIdx<0 means "don't include in RTP header"; temporalIdx=0 = base layer.
        info.codecSpecific.VP8.keyIdx      = -1;
        info.codecSpecific.VP8.temporalIdx = webrtc::kNoTemporalIdx;
        info.codecSpecific.VP8.layerSync   = false;
        info.codecSpecific.VP8.nonReference = false;
        break;
      case webrtc::kVideoCodecVP9:
        info.codecSpecific.VP9.inter_pic_predicted   = !out.is_key_frame;
        info.codecSpecific.VP9.first_frame_in_picture = true;
        info.codecSpecific.VP9.num_spatial_layers     = 1;
        info.codecSpecific.VP9.temporal_idx           = webrtc::kNoTemporalIdx;
        info.codecSpecific.VP9.temporal_up_switch     = false;
        info.codecSpecific.VP9.ss_data_available      = false;
        break;
      default:
        // AV1, H265, Generic: codecType is sufficient; no union fields needed.
        break;
    }

    callback_->OnEncodedImage(img, &info);
    // Release the encoded buffer now that OnEncodedImage has copied it.
    if (out.free_data) out.free_data(out.data, out.len);
    return WEBRTC_VIDEO_CODEC_OK;
  }

  void SetRates(const RateControlParameters&) override {}

  EncoderInfo GetEncoderInfo() const override {
    EncoderInfo info;
    info.implementation_name     = "ReactorCustom";
    info.is_hardware_accelerated = true;
    return info;
  }
};

class ReactorVideoEncoderFactory : public webrtc::VideoEncoderFactory {
  std::shared_ptr<ReactorEncoderState>        state_;
  std::atomic<uint64_t>                       next_id_{0};
  std::unique_ptr<webrtc::VideoEncoderFactory> builtin_;

 public:
  explicit ReactorVideoEncoderFactory(std::shared_ptr<ReactorEncoderState> s)
      : state_(std::move(s)),
        builtin_(webrtc::CreateBuiltinVideoEncoderFactory()) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return {
        webrtc::SdpVideoFormat::VP8(),
        webrtc::SdpVideoFormat::VP9Profile0(),
        webrtc::SdpVideoFormat("H264", {{"level-asymmetry-allowed", "1"},
                                         {"packetization-mode", "1"},
                                         {"profile-level-id", "42e01f"}}),
        webrtc::SdpVideoFormat::AV1Profile0(),
        webrtc::SdpVideoFormat::H265(),
    };
  }

  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    uint64_t id = next_id_.fetch_add(1, std::memory_order_relaxed);
    if (state_->use_builtin && state_->use_builtin(state_->userdata, id)) {
      return builtin_->Create(env, format);
    }
    return std::make_unique<ReactorVideoEncoder>(state_, id);
  }
};

// A no-op decoder for codecs not present in the builtin factory (H264, H265 in
// this build which was compiled without WEBRTC_USE_H264). It claims support so
// that SDP negotiation succeeds with peers that only advertise those codecs, but
// discards every received frame. Right for send-only sessions or media servers.
class ReactorNullVideoDecoder : public webrtc::VideoDecoder {
  std::string name_;
 public:
  explicit ReactorNullVideoDecoder(std::string name) : name_(std::move(name)) {}
  bool Configure(const Settings&) override { return true; }
  int32_t Decode(const webrtc::EncodedImage&, int64_t) override { return 0; }
  int32_t RegisterDecodeCompleteCallback(webrtc::DecodedImageCallback*) override {
    return 0;
  }
  int32_t Release() override { return 0; }
  DecoderInfo GetDecoderInfo() const override {
    return {name_, /*is_hardware_accelerated=*/false};
  }
};

// Wraps the builtin video decoder factory and adds null decoders for H264 and
// H265, which this libwebrtc build (compiled without WEBRTC_USE_H264) does not
// include. VP8, VP9, and AV1 are handled by the builtin factory as usual.
class ReactorCustomDecoderFactory : public webrtc::VideoDecoderFactory {
  std::unique_ptr<webrtc::VideoDecoderFactory> builtin_;

  static bool IsBuiltinCodec(const webrtc::SdpVideoFormat& f) {
    return f.name == "VP8" || f.name == "VP9" || f.name == "AV1";
  }

 public:
  ReactorCustomDecoderFactory()
      : builtin_(webrtc::CreateBuiltinVideoDecoderFactory()) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    auto formats = builtin_->GetSupportedFormats();
    formats.push_back(webrtc::SdpVideoFormat(
        "H264", {{"level-asymmetry-allowed", "1"},
                 {"packetization-mode", "1"},
                 {"profile-level-id", "42e01f"}}));
    formats.push_back(webrtc::SdpVideoFormat::H265());
    return formats;
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (IsBuiltinCodec(format)) return builtin_->Create(env, format);
    return std::make_unique<ReactorNullVideoDecoder>("ReactorNull_" + format.name);
  }
};

// Create a PeerConnectionFactory that routes all video encoding through `cb`.
// `cb` is called synchronously inside VideoEncoder::Encode() with the raw I420
// frame; fill `*out` and return 0 to inject bytes into the RTP stack, or
// return non-zero to drop. `free_ud` is called when all encoder instances are
// gone (follows the same lifetime contract as frame_transformer_create).
void* reactor_webrtc_factory_create_with_custom_video_encoder(
    int use_platform_adm, reactor_video_encode_cb cb, void* userdata,
    reactor_webrtc_userdata_free free_ud, reactor_use_builtin_cb use_builtin) {
  // make_shared constructs in-place via the explicit ctor — no temporary,
  // no copy, no premature destructor call.
  auto state = std::make_shared<ReactorEncoderState>(cb, userdata, free_ud, use_builtin);

  auto f = std::make_unique<ReactorFactory>();
  f->network_thread   = webrtc::Thread::CreateWithSocketServer();
  f->worker_thread    = webrtc::Thread::Create();
  f->signaling_thread = webrtc::Thread::Create();
  if (!f->network_thread->Start() || !f->worker_thread->Start() ||
      !f->signaling_thread->Start()) {
    return nullptr;
  }

  webrtc::scoped_refptr<webrtc::AudioDeviceModule> adm;
  webrtc::scoped_refptr<webrtc::AudioProcessing>   apm;
  if (use_platform_adm) {
    webrtc::AudioProcessing::Config apm_config;
    apm_config.echo_canceller.enabled    = true;
    apm_config.noise_suppression.enabled = true;
    apm_config.noise_suppression.level   =
        webrtc::AudioProcessing::Config::NoiseSuppression::kHigh;
    apm_config.gain_controller1.enabled  = true;
    apm_config.high_pass_filter.enabled  = true;
    apm = webrtc::BuiltinAudioProcessingBuilder(apm_config)
              .Build(webrtc::CreateEnvironment());
  } else {
    f->adm = webrtc::make_ref_counted<FrameAdm>();
    adm    = f->adm;
  }

  f->factory = webrtc::CreatePeerConnectionFactory(
      f->network_thread.get(), f->worker_thread.get(),
      f->signaling_thread.get(), adm,
      webrtc::CreateBuiltinAudioEncoderFactory(),
      webrtc::CreateBuiltinAudioDecoderFactory(),
      std::make_unique<ReactorVideoEncoderFactory>(state),
      std::make_unique<ReactorCustomDecoderFactory>(),
      /*audio_mixer=*/nullptr, /*audio_processing=*/apm);
  if (!f->factory) return nullptr;
  return f.release();
}

// Replace the encoded payload of the frame currently in the callback. Copies.
void reactor_webrtc_encoded_frame_set_data(void* frame, const uint8_t* data,
                                           size_t len) {
  if (!frame || !data) return;
  auto* f = reinterpret_cast<webrtc::TransformableFrameInterface*>(frame);
  f->SetData(std::span<const uint8_t>(data, len));
}

class ReactorFrameTransformer : public webrtc::FrameTransformerInterface {
 public:
  ReactorFrameTransformer(reactor_webrtc_encoded_frame_cb cb, void* userdata,
                          reactor_webrtc_userdata_free free_ud)
      : cb_(cb), userdata_(userdata), free_ud_(free_ud) {}
  // Runs when the last ref drops (senders/receivers hold their own), so the
  // binding's userdata outlives every possible callback.
  ~ReactorFrameTransformer() override {
    if (free_ud_) free_ud_(userdata_);
  }

  // Single callback: used by senders (one transform per sender).
  void RegisterTransformedFrameCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> cb) override {
    std::lock_guard<std::mutex> lock(mu_);
    send_sink_ = std::move(cb);
  }
  void UnregisterTransformedFrameCallback() override {
    std::lock_guard<std::mutex> lock(mu_);
    send_sink_ = nullptr;
  }
  // Per-ssrc callback: used by receivers (one transform can serve many ssrcs).
  void RegisterTransformedFrameSinkCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> cb,
      uint32_t ssrc) override {
    std::lock_guard<std::mutex> lock(mu_);
    recv_sinks_[ssrc] = std::move(cb);
  }
  void UnregisterTransformedFrameSinkCallback(uint32_t ssrc) override {
    std::lock_guard<std::mutex> lock(mu_);
    recv_sinks_.erase(ssrc);
  }

  void Transform(
      std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    if (!frame) return;
    // Pick the sink for this frame: receive frames route by ssrc, send frames
    // use the single sender sink.
    webrtc::scoped_refptr<webrtc::TransformedFrameCallback> sink;
    {
      std::lock_guard<std::mutex> lock(mu_);
      auto it = recv_sinks_.find(frame->GetSsrc());
      if (it != recv_sinks_.end())
        sink = it->second;
      else
        sink = send_sink_;
    }

    int emit = 0;
    if (cb_) {
      // RTTI is on (use_rtti=true), so dynamic_cast distinguishes video/audio
      // and exposes IsKeyFrame.
      int is_audio = 0, is_key = 0;
      if (auto* v = dynamic_cast<webrtc::TransformableVideoFrameInterface*>(
              frame.get())) {
        is_key = v->IsKeyFrame() ? 1 : 0;
      } else if (dynamic_cast<webrtc::TransformableAudioFrameInterface*>(
                     frame.get())) {
        is_audio = 1;
      }
      const std::span<const uint8_t> data = frame->GetData();
      const std::string mime = frame->GetMimeType();
      const int direction =
          frame->GetDirection() ==
                  webrtc::TransformableFrameInterface::Direction::kReceiver
              ? 1
              : 0;
      ReactorEncodedFrame ef{
          direction,
          is_audio,
          is_key,
          frame->GetPayloadType(),
          frame->GetSsrc(),
          frame->GetTimestamp(),
          data.data(),
          data.size(),
          mime.c_str(),
          frame.get(),
      };
      emit = cb_(userdata_, &ef);
    }

    if (emit == 0 && sink) sink->OnTransformedFrame(std::move(frame));
    // else: dropped (decoder bypassed on receive; nothing sent on egress).
  }

 private:
  reactor_webrtc_encoded_frame_cb cb_;
  void* userdata_;
  reactor_webrtc_userdata_free free_ud_;
  std::mutex mu_;
  webrtc::scoped_refptr<webrtc::TransformedFrameCallback> send_sink_;
  std::map<uint32_t, webrtc::scoped_refptr<webrtc::TransformedFrameCallback>>
      recv_sinks_;
};

// Owns one reference to the transformer (the sender/receiver keep their own).
struct ReactorTransformerHandle {
  webrtc::scoped_refptr<ReactorFrameTransformer> t;
};

// Create an encoded-frame transformer. `cb` fires per encoded frame (see
// ReactorEncodedFrame); `free_ud` (nullable) frees `userdata` when the
// transformer is finally destroyed. Returns an owned handle
// (free with reactor_webrtc_frame_transformer_destroy) or null.
void* reactor_webrtc_frame_transformer_create(
    reactor_webrtc_encoded_frame_cb cb, void* userdata,
    reactor_webrtc_userdata_free free_ud) {
  auto t = webrtc::make_ref_counted<ReactorFrameTransformer>(cb, userdata,
                                                             free_ud);
  return new ReactorTransformerHandle{std::move(t)};
}

// Attach the transformer to the transceiver's sender (encoder -> packetizer).
// Returns 1 on success, 0 on failure.
int reactor_webrtc_rtp_transceiver_set_sender_transform(void* transceiver,
                                                        void* transformer) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  auto* th = reinterpret_cast<ReactorTransformerHandle*>(transformer);
  if (!h || !h->tc || !th || !h->tc->sender()) return 0;
  h->tc->sender()->SetFrameTransformer(th->t);
  return 1;
}

// Attach the transformer to the transceiver's receiver (depacketizer ->
// decoder). Returns 1 on success, 0 on failure.
int reactor_webrtc_rtp_transceiver_set_receiver_transform(void* transceiver,
                                                          void* transformer) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  auto* th = reinterpret_cast<ReactorTransformerHandle*>(transformer);
  if (!h || !h->tc || !th || !h->tc->receiver()) return 0;
  h->tc->receiver()->SetFrameTransformer(th->t);
  return 1;
}

// Release a transformer handle (our reference; sender/receiver keep theirs).
void reactor_webrtc_frame_transformer_destroy(void* transformer) {
  delete reinterpret_cast<ReactorTransformerHandle*>(transformer);
}

// Destroy a track handle (detaches any sink and releases the track + source).
void reactor_webrtc_media_stream_track_destroy(void* track) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (h && h->track) {
    if (h->sink && h->track->kind() == "video") {
      static_cast<webrtc::VideoTrackInterface*>(h->track.get())
          ->RemoveSink(h->sink.get());
    }
    if (h->audio_sink && h->track->kind() == "audio") {
      static_cast<webrtc::AudioTrackInterface*>(h->track.get())
          ->RemoveSink(h->audio_sink.get());
    }
  }
  delete h;
}

}  // extern "C"
