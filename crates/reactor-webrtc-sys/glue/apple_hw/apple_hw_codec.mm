#include "apple_hw_codec.h"

#include <utility>
#include <vector>

#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/sdp_video_format.h"

#import "sdk/objc/components/video_codec/RTCVideoDecoderFactoryH264.h"
#import "sdk/objc/components/video_codec/RTCVideoEncoderFactoryH264.h"
#include "sdk/objc/native/api/video_decoder_factory.h"
#include "sdk/objc/native/api/video_encoder_factory.h"

namespace reactor {
namespace {

bool IsH264(const webrtc::SdpVideoFormat& format) {
  return format.name == "H264";
}

// Wraps the builtin video encoder factory (VP8/VP9/AV1 unchanged) and
// delegates H264 to the real VideoToolbox-backed factory. Mirrors
// glue/openh264/openh264_codec.{h,cc} and
// glue/android_hw/android_hw_codec.{h,cc}'s wrap-and-delegate shape.
class AppleHwVideoEncoderFactory : public webrtc::VideoEncoderFactory {
 public:
  explicit AppleHwVideoEncoderFactory(std::unique_ptr<webrtc::VideoEncoderFactory> hw)
      : builtin_(webrtc::CreateBuiltinVideoEncoderFactory()), hw_(std::move(hw)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    std::vector<webrtc::SdpVideoFormat> formats = builtin_->GetSupportedFormats();
    for (const auto& format : hw_->GetSupportedFormats()) {
      if (IsH264(format)) formats.push_back(format);
    }
    return formats;
  }

  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& env, const webrtc::SdpVideoFormat& format) override {
    if (IsH264(format)) return hw_->Create(env, format);
    return builtin_->Create(env, format);
  }

 private:
  std::unique_ptr<webrtc::VideoEncoderFactory> builtin_;
  std::unique_ptr<webrtc::VideoEncoderFactory> hw_;
};

class AppleHwVideoDecoderFactory : public webrtc::VideoDecoderFactory {
 public:
  explicit AppleHwVideoDecoderFactory(std::unique_ptr<webrtc::VideoDecoderFactory> hw)
      : builtin_(webrtc::CreateBuiltinVideoDecoderFactory()), hw_(std::move(hw)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    std::vector<webrtc::SdpVideoFormat> formats = builtin_->GetSupportedFormats();
    for (const auto& format : hw_->GetSupportedFormats()) {
      if (IsH264(format)) formats.push_back(format);
    }
    return formats;
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& env, const webrtc::SdpVideoFormat& format) override {
    if (IsH264(format)) return hw_->Create(env, format);
    return builtin_->Create(env, format);
  }

 private:
  std::unique_ptr<webrtc::VideoDecoderFactory> builtin_;
  std::unique_ptr<webrtc::VideoDecoderFactory> hw_;
};

}  // namespace

std::unique_ptr<webrtc::VideoEncoderFactory> CreateAppleHwVideoEncoderFactory() {
  id<RTC_OBJC_TYPE(RTCVideoEncoderFactory)> objc_factory =
      [[RTC_OBJC_TYPE(RTCVideoEncoderFactoryH264) alloc] init];
  return std::make_unique<AppleHwVideoEncoderFactory>(
      webrtc::ObjCToNativeVideoEncoderFactory(objc_factory));
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateAppleHwVideoDecoderFactory() {
  id<RTC_OBJC_TYPE(RTCVideoDecoderFactory)> objc_factory =
      [[RTC_OBJC_TYPE(RTCVideoDecoderFactoryH264) alloc] init];
  return std::make_unique<AppleHwVideoDecoderFactory>(
      webrtc::ObjCToNativeVideoDecoderFactory(objc_factory));
}

}  // namespace reactor
