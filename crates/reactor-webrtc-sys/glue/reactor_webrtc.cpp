// reactor-webrtc glue — the C++ side of the FFI boundary.
//
// M1: the first real objects backed by our owned libwebrtc.a. Builds the
// builtin audio/video codec factories, enumerates them (link/run self-test),
// and creates a real PeerConnectionFactory (threads + media engine).
//
// The builtin codec factories live in their own gn targets that the `webrtc`
// umbrella normally omits; webrtc-build/patches/0001-* injects them into the
// umbrella's deps so these symbols resolve in libwebrtc.a.
//
// The rest of the object surface declared in `src/lib.rs` (peer connection,
// tracks, frame I/O, ADM) lands as the build pipeline matures and will be
// generated rather than hand-written.

#include <cstring>
#include <memory>
#include <string>

#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/create_peerconnection_factory.h"
#include "api/peer_connection_interface.h"
#include "api/scoped_refptr.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "rtc_base/thread.h"

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

}  // extern "C"
