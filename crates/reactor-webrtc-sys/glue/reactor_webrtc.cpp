// reactor-webrtc glue — the C++ side of the FFI boundary.
//
// M1 scaffold: a minimal but *real* slice that proves our owned libwebrtc.a
// links and runs. It instantiates WebRTC's builtin audio + video encoder
// factories and enumerates the codecs they register — exercising a large chunk
// of the library (codec registration, abseil, scoped_refptr) without touching
// audio/video hardware.
//
// The builtin video encoder/decoder factories live in their own gn targets
// (api/video_codecs:builtin_video_{en,de}coder_factory) that the `webrtc`
// umbrella does not pull in; webrtc-build/build.sh builds them and merges their
// objects into libwebrtc.a so the symbols below resolve.
//
// The full object surface declared in `src/lib.rs` (factory, peer connection,
// tracks, frame I/O, ADM) lands as the build pipeline matures and will be
// generated rather than hand-written.

#include <cstring>
#include <memory>
#include <string>

#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/scoped_refptr.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"

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

}  // extern "C"
