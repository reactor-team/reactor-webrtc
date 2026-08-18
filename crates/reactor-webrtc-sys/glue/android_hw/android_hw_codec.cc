#include "android_hw_codec.h"

#include <string>
#include <utility>
#include <vector>

#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/sdp_video_format.h"
#include "rtc_base/logging.h"
#include "sdk/android/native_api/codecs/wrapper.h"

namespace reactor {
namespace {

// Namespaced per webrtc-build/patches/0002-android-jni-package-prefix.patch
// (android_jni_package_prefix="inc.reactor").
constexpr char kHwEncoderFactoryClass[] =
    "inc/reactor/org/webrtc/HardwareVideoEncoderFactory";
constexpr char kHwDecoderFactoryClass[] =
    "inc/reactor/org/webrtc/HardwareVideoDecoderFactory";
constexpr char kEglBaseContextClass[] = "inc/reactor/org/webrtc/EglBase$Context";

bool IsH264(const webrtc::SdpVideoFormat& format) {
  return format.name == "H264";
}

webrtc::SdpVideoFormat H264Format() {
  return webrtc::SdpVideoFormat("H264", {{"level-asymmetry-allowed", "1"},
                                          {"packetization-mode", "1"},
                                          {"profile-level-id", "42e01f"}});
}

// Logs and clears a pending JNI exception (FindClass/GetMethodID/NewObject
// all leave one pending rather than returning an error code). Returns true if
// one was pending.
bool ClearPendingException(JNIEnv* jni, const char* what) {
  if (!jni->ExceptionCheck()) return false;
  RTC_LOG(LS_WARNING) << "reactor: android_hw_codec: JNI exception in " << what;
  jni->ExceptionDescribe();
  jni->ExceptionClear();
  return true;
}

// Wraps the builtin video encoder factory (VP8/VP9/AV1 unchanged) and, if
// `hw` is non-null, delegates H264 to it. Mirrors
// glue/openh264/openh264_codec.{h,cc}'s wrap-and-delegate shape.
class AndroidHwVideoEncoderFactory : public webrtc::VideoEncoderFactory {
 public:
  explicit AndroidHwVideoEncoderFactory(
      std::unique_ptr<webrtc::VideoEncoderFactory> hw)
      : builtin_(webrtc::CreateBuiltinVideoEncoderFactory()), hw_(std::move(hw)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    std::vector<webrtc::SdpVideoFormat> formats = builtin_->GetSupportedFormats();
    if (hw_) formats.push_back(H264Format());
    return formats;
  }

  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (hw_ && IsH264(format)) return hw_->Create(env, format);
    return builtin_->Create(env, format);
  }

 private:
  std::unique_ptr<webrtc::VideoEncoderFactory> builtin_;
  std::unique_ptr<webrtc::VideoEncoderFactory> hw_;
};

class AndroidHwVideoDecoderFactory : public webrtc::VideoDecoderFactory {
 public:
  explicit AndroidHwVideoDecoderFactory(
      std::unique_ptr<webrtc::VideoDecoderFactory> hw)
      : builtin_(webrtc::CreateBuiltinVideoDecoderFactory()), hw_(std::move(hw)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    std::vector<webrtc::SdpVideoFormat> formats = builtin_->GetSupportedFormats();
    if (hw_) formats.push_back(H264Format());
    return formats;
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (hw_ && IsH264(format)) return hw_->Create(env, format);
    return builtin_->Create(env, format);
  }

 private:
  std::unique_ptr<webrtc::VideoDecoderFactory> builtin_;
  std::unique_ptr<webrtc::VideoDecoderFactory> hw_;
};

// Constructs `class_name`'s Java object via a constructor taking exactly
// (EglBase.Context, boolean, boolean), passing (null, false, false) -- no
// shared EGL context (I420 path, not texture), Intel VP8 and H264-high-profile
// both off (matches HardwareVideoDecoderFactory's own simplest ctor shape;
// HardwareVideoEncoderFactory needs the 3-arg form specifically). Returns a
// local ref, or null (with the JNI exception already cleared) on any failure.
jobject NewEncoderFactory(JNIEnv* jni) {
  jclass klass = jni->FindClass(kHwEncoderFactoryClass);
  if (ClearPendingException(jni, "FindClass(HardwareVideoEncoderFactory)") || !klass) {
    return nullptr;
  }
  std::string sig = "(L" + std::string(kEglBaseContextClass) + ";ZZ)V";
  jmethodID ctor = jni->GetMethodID(klass, "<init>", sig.c_str());
  if (ClearPendingException(jni, "GetMethodID(HardwareVideoEncoderFactory.<init>)") ||
      !ctor) {
    jni->DeleteLocalRef(klass);
    return nullptr;
  }
  jobject obj = jni->NewObject(klass, ctor, /*sharedContext=*/nullptr,
                                /*enableIntelVp8Encoder=*/JNI_FALSE,
                                /*enableH264HighProfile=*/JNI_FALSE);
  ClearPendingException(jni, "NewObject(HardwareVideoEncoderFactory)");
  jni->DeleteLocalRef(klass);
  return obj;
}

// Same idea for the decoder, which has a simpler single-arg ctor
// (EglBase.Context) -- null again for the same reason.
jobject NewDecoderFactory(JNIEnv* jni) {
  jclass klass = jni->FindClass(kHwDecoderFactoryClass);
  if (ClearPendingException(jni, "FindClass(HardwareVideoDecoderFactory)") || !klass) {
    return nullptr;
  }
  std::string sig = "(L" + std::string(kEglBaseContextClass) + ";)V";
  jmethodID ctor = jni->GetMethodID(klass, "<init>", sig.c_str());
  if (ClearPendingException(jni, "GetMethodID(HardwareVideoDecoderFactory.<init>)") ||
      !ctor) {
    jni->DeleteLocalRef(klass);
    return nullptr;
  }
  jobject obj = jni->NewObject(klass, ctor, /*sharedContext=*/nullptr);
  ClearPendingException(jni, "NewObject(HardwareVideoDecoderFactory)");
  jni->DeleteLocalRef(klass);
  return obj;
}

}  // namespace

std::unique_ptr<webrtc::VideoEncoderFactory> CreateAndroidHwVideoEncoderFactory(
    JNIEnv* jni) {
  std::unique_ptr<webrtc::VideoEncoderFactory> hw;
  if (jobject java_factory = NewEncoderFactory(jni)) {
    hw = webrtc::JavaToNativeVideoEncoderFactory(jni, java_factory);
    jni->DeleteLocalRef(java_factory);
  } else {
    RTC_LOG(LS_WARNING) << "reactor: " << kHwEncoderFactoryClass
                         << " unavailable -- H264 encode not advertised "
                            "(is libwebrtc.jar on the app's classpath?)";
  }
  return std::make_unique<AndroidHwVideoEncoderFactory>(std::move(hw));
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateAndroidHwVideoDecoderFactory(
    JNIEnv* jni) {
  std::unique_ptr<webrtc::VideoDecoderFactory> hw;
  if (jobject java_factory = NewDecoderFactory(jni)) {
    hw = webrtc::JavaToNativeVideoDecoderFactory(jni, java_factory);
    jni->DeleteLocalRef(java_factory);
  } else {
    RTC_LOG(LS_WARNING) << "reactor: " << kHwDecoderFactoryClass
                         << " unavailable -- H264 decode not advertised "
                            "(is libwebrtc.jar on the app's classpath?)";
  }
  return std::make_unique<AndroidHwVideoDecoderFactory>(std::move(hw));
}

}  // namespace reactor
