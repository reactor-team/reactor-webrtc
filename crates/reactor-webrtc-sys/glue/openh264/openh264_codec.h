// Real H.264 encode/decode backed by a dynamically loaded Cisco OpenH264
// shared library — see ../../src/openh264.rs for why this is dlopen'd rather
// than compiled in, and vendor/README.md for the vendored ABI headers this
// depends on.
#pragma once

#include <memory>
#include <string>

#include "api/video_codecs/video_decoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"

// Forward-declared rather than including vendor/codec_api.h here, so this
// header doesn't force OpenH264's ABI declarations onto everything that
// merely wants to hold/pass an OpenH264Library — only openh264_codec.cc
// needs the full definitions to actually call through the vtable.
class ISVCEncoder;
class ISVCDecoder;

namespace reactor {

// RAII wrapper around a dlopen'd/LoadLibraryW'd Cisco OpenH264 shared
// library. Resolves WelsCreateSVCEncoder/WelsDestroySVCEncoder/
// WelsCreateDecoder/WelsDestroyDecoder at construction time; `ok()` reports
// whether every symbol was found. Never throws — a failed load just leaves
// `ok() == false`, which the factories below treat as "no H.264 available"
// and degrade rather than crash.
class OpenH264Library {
 public:
  static std::unique_ptr<OpenH264Library> Open(const std::string& path);
  ~OpenH264Library();

  OpenH264Library(const OpenH264Library&) = delete;
  OpenH264Library& operator=(const OpenH264Library&) = delete;

  bool ok() const { return ok_; }

  // Returns null on failure (mirrors WelsCreate*'s own null-on-failure
  // contract, and `!ok()` short-circuits without touching the library).
  ISVCEncoder* CreateEncoder() const;
  void DestroyEncoder(ISVCEncoder* encoder) const;
  ISVCDecoder* CreateDecoder() const;
  void DestroyDecoder(ISVCDecoder* decoder) const;

 private:
  OpenH264Library() = default;

  void* handle_ = nullptr;
  bool ok_ = false;
  int (*create_encoder_)(ISVCEncoder**) = nullptr;
  void (*destroy_encoder_)(ISVCEncoder*) = nullptr;
  long (*create_decoder_)(ISVCDecoder**) = nullptr;
  void (*destroy_decoder_)(ISVCDecoder*) = nullptr;
};

// Wraps the builtin video encoder/decoder factories (VP8/VP9/AV1 unchanged)
// and adds a real H.264 codec backed by `lib`. `lib` is shared between both
// factories and every encoder/decoder instance they create, since it owns
// the loaded shared library those instances call into.
//
// If `lib` is null or `!lib->ok()`, H.264 is simply not added to
// GetSupportedFormats() — these factories behave exactly like the plain
// builtin ones, so a peer never negotiates a codec this process can't
// actually run.
std::unique_ptr<webrtc::VideoEncoderFactory> CreateOpenH264VideoEncoderFactory(
    std::shared_ptr<OpenH264Library> lib);
std::unique_ptr<webrtc::VideoDecoderFactory> CreateOpenH264VideoDecoderFactory(
    std::shared_ptr<OpenH264Library> lib);

}  // namespace reactor
