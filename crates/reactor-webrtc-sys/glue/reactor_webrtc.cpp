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

#include <cstring>
#include <memory>
#include <string>
#include <utility>

#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/create_peerconnection_factory.h"
#include "api/data_channel_interface.h"
#include "api/jsep.h"
#include "api/make_ref_counted.h"
#include "api/peer_connection_interface.h"
#include "api/rtc_error.h"
#include "api/scoped_refptr.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "rtc_base/thread.h"

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
};
}  // extern "C"

namespace {

// Owns the three WebRTC threads alongside the factory. The threads must outlive
// the factory, so declare `factory` last: members are destroyed in reverse
// declaration order, releasing the factory before the threads stop.
struct ReactorFactory {
  std::unique_ptr<webrtc::Thread> network_thread;
  std::unique_ptr<webrtc::Thread> worker_thread;
  std::unique_ptr<webrtc::Thread> signaling_thread;
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

// Forwards CreateOffer results to C callbacks.
class CreateOfferObserver : public webrtc::CreateSessionDescriptionObserver {
 public:
  CreateOfferObserver(void* userdata,
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

  f->factory = webrtc::CreatePeerConnectionFactory(
      f->network_thread.get(), f->worker_thread.get(),
      f->signaling_thread.get(),
      /*default_adm=*/nullptr, webrtc::CreateBuiltinAudioEncoderFactory(),
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
  auto observer = webrtc::make_ref_counted<CreateOfferObserver>(
      userdata, on_success, on_error);
  webrtc::PeerConnectionInterface::RTCOfferAnswerOptions options;
  rpc->pc->CreateOffer(observer.get(), options);
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

}  // extern "C"
