// reactor-webrtc glue — the C++ side of the FFI boundary.
//
// M1 scaffold: a minimal but *real* slice that proves our owned libwebrtc.a
// links and runs. It instantiates WebRTC's builtin audio encoder factory and
// enumerates the codecs it registers — exercising a large chunk of the library
// (codec registration, abseil, scoped_refptr) without touching audio hardware.
//
// (The builtin *video* encoder factory lives in a separate gn target that the
// `webrtc` umbrella does not pull in, so it is intentionally not used here.)
//
// The full object surface declared in `src/lib.rs` (factory, peer connection,
// tracks, frame I/O, ADM) lands as the build pipeline matures and will be
// generated rather than hand-written.

#include <cstring>
#include <string>

#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/scoped_refptr.h"

extern "C" {

// ABI version of this native build. The safe crate asserts compatibility.
unsigned int reactor_webrtc_abi_version() { return 1; }

// Link/run self-test: build the builtin audio encoder factory and enumerate
// the codecs it supports. Writes a comma-separated, NUL-terminated list of
// codec names into `out` (truncated to `cap`) and returns the count. A
// non-zero return means real libwebrtc code linked and executed.
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

  if (out && cap > 0) {
    std::strncpy(out, names.c_str(), static_cast<size_t>(cap) - 1);
    out[cap - 1] = '\0';
  }
  return count;
}

}  // extern "C"
