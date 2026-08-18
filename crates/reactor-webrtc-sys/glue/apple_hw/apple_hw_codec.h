// Real H.264 encode/decode on macOS/iOS via VideoToolbox, bridged in through
// WebRTC's own Objective-C RTCVideoEncoderFactoryH264/RTCVideoDecoderFactoryH264
// (sdk:videotoolbox_objc) and ObjCToNativeVideo{Encoder,Decoder}Factory
// (sdk:native_api) -- see webrtc-build/patches/0004-webrtc-add-apple-videotoolbox-h264.patch
// for the umbrella deps that land these objects in libwebrtc.a.
//
// This header is plain C++ (no Objective-C types in the interface) so it can
// be included from reactor_webrtc.cpp; apple_hw_codec.mm holds the
// Objective-C++ bridging code.
#pragma once

#include <memory>

#include "api/video_codecs/video_decoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"

namespace reactor {

// Both wrap the builtin factory (VP8/VP9/AV1 unchanged) and delegate H264 to
// the real VideoToolbox-backed factory -- same wrap-and-delegate shape as
// glue/openh264/openh264_codec.{h,cc} and glue/android_hw/android_hw_codec.{h,cc}.
std::unique_ptr<webrtc::VideoEncoderFactory> CreateAppleHwVideoEncoderFactory();
std::unique_ptr<webrtc::VideoDecoderFactory> CreateAppleHwVideoDecoderFactory();

}  // namespace reactor
