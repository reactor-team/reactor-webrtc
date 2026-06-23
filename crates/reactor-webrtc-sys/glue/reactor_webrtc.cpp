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
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "api/audio/audio_device.h"
#include "api/audio/audio_device_defines.h"
#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/audio_options.h"
#include "api/create_peerconnection_factory.h"
#include "api/data_channel_interface.h"
#include "api/jsep.h"
#include "api/make_ref_counted.h"
#include "api/media_stream_interface.h"
#include "api/peer_connection_interface.h"
#include "api/rtc_error.h"
#include "api/rtp_receiver_interface.h"
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
#include "pc/video_track_source.h"
#include "rtc_base/thread.h"
#include "rtc_base/time_utils.h"
#include "third_party/libyuv/include/libyuv/convert.h"

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
    return 0;
  }
  bool RecordingIsInitialized() const override { return true; }
  bool Recording() const override { return true; }

  // Playout pump: pulls (and discards) 10ms render blocks so the receive
  // pipeline runs and remote audio-track sinks are invoked.
  int32_t StartPlayout() override {
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
    while (playing_.load()) {
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (transport_) {
          size_t out = 0;
          int64_t elapsed = 0, ntp = 0;
          transport_->NeedMorePlayData(frames, sizeof(int16_t) * channels,
                                       channels, rate, scratch.data(), out,
                                       &elapsed, &ntp);
        }
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
  }

  std::mutex mutex_;
  webrtc::AudioTransport* transport_ = nullptr;
  std::atomic<bool> playing_{false};
  std::thread play_thread_;
};

// Bridges decoded frames from a (remote) audio track to a C callback.
class AudioFrameSink : public webrtc::AudioTrackSinkInterface {
 public:
  AudioFrameSink(void* userdata,
                 void (*on_audio)(void*, int, int, int))
      : userdata_(userdata), on_audio_(on_audio) {}
  void OnData(const void* /*audio_data*/, int /*bits_per_sample*/,
              int sample_rate, size_t number_of_channels,
              size_t number_of_frames) override {
    if (on_audio_)
      on_audio_(userdata_, sample_rate, static_cast<int>(number_of_channels),
                static_cast<int>(number_of_frames));
  }

 private:
  void* userdata_;
  void (*on_audio_)(void*, int, int, int);
};

// Bridges decoded frames from a (remote) video track to a C callback.
class FrameSink : public webrtc::VideoSinkInterface<webrtc::VideoFrame> {
 public:
  FrameSink(void* userdata, void (*on_frame)(void*, int, int))
      : userdata_(userdata), on_frame_(on_frame) {}
  void OnFrame(const webrtc::VideoFrame& frame) override {
    if (on_frame_) on_frame_(userdata_, frame.width(), frame.height());
  }

 private:
  void* userdata_;
  void (*on_frame_)(void*, int, int);
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
    cb_.on_data_channel(
        cb_.userdata,
        new webrtc::scoped_refptr<webrtc::DataChannelInterface>(std::move(dc)));
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
// and wire the builtin audio+video codec factories. The default ADM (nullptr)
// is created by the media engine; a synthetic/headless ADM hook lands later.
// Returns an opaque ReactorFactory* (the `PeerConnectionFactory` handle in the
// Rust API), or nullptr on failure.
void* reactor_webrtc_factory_create() {
  auto f = std::make_unique<ReactorFactory>();

  f->network_thread = webrtc::Thread::CreateWithSocketServer();
  f->worker_thread = webrtc::Thread::Create();
  f->signaling_thread = webrtc::Thread::Create();
  if (!f->network_thread->Start() || !f->worker_thread->Start() ||
      !f->signaling_thread->Start()) {
    return nullptr;
  }

  f->adm = webrtc::make_ref_counted<FrameAdm>();
  f->factory = webrtc::CreatePeerConnectionFactory(
      f->network_thread.get(), f->worker_thread.get(),
      f->signaling_thread.get(), f->adm,
      webrtc::CreateBuiltinAudioEncoderFactory(),
      webrtc::CreateBuiltinAudioDecoderFactory(),
      webrtc::CreateBuiltinVideoEncoderFactory(),
      webrtc::CreateBuiltinVideoDecoderFactory(),
      /*audio_mixer=*/nullptr, /*audio_processing=*/nullptr);
  if (!f->factory) {
    return nullptr;  // threads stopped by ReactorFactory's destructor
  }
  return f.release();
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
  auto result =
      rpc->pc->CreateDataChannelOrError(label ? label : "", &init);
  if (!result.ok()) return nullptr;
  return new webrtc::scoped_refptr<webrtc::DataChannelInterface>(
      result.MoveValue());
}

// Destroy a DataChannel handle (releases our reference).
void reactor_webrtc_data_channel_destroy(void* data_channel) {
  delete reinterpret_cast<webrtc::scoped_refptr<webrtc::DataChannelInterface>*>(
      data_channel);
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
void reactor_webrtc_audio_track_add_sink(void* track, void* userdata,
                                         void (*on_audio)(void*, int, int,
                                                          int)) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track || h->track->kind() != "audio") return;
  h->audio_sink = std::make_unique<AudioFrameSink>(userdata, on_audio);
  static_cast<webrtc::AudioTrackInterface*>(h->track.get())
      ->AddSink(h->audio_sink.get());
}

// Attach a frame sink to a (video) track. `on_frame(userdata, width, height)`
// fires per decoded frame. The sink lives until the track handle is destroyed.
void reactor_webrtc_video_track_add_sink(void* track, void* userdata,
                                         void (*on_frame)(void*, int, int)) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track || h->track->kind() != "video") return;
  h->sink = std::make_unique<FrameSink>(userdata, on_frame);
  static_cast<webrtc::VideoTrackInterface*>(h->track.get())
      ->AddOrUpdateSink(h->sink.get(), webrtc::VideoSinkWants());
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
