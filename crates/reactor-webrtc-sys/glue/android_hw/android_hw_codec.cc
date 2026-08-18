#include "android_hw_codec.h"

#include <string>
#include <utility>
#include <vector>

#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/sdp_video_format.h"
#include "rtc_base/logging.h"
#include "sdk/android/native_api/codecs/wrapper.h"
#include "sdk/android/native_api/jni/class_loader.h"

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
    if (hw_) {
      // Only advertise the H264 profiles the device's MediaCodec allowlist
      // actually reports as supported -- not every device does.
      for (const auto& format : hw_->GetSupportedFormats()) {
        if (IsH264(format)) formats.push_back(format);
      }
    }
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
    if (hw_) {
      // Only advertise the H264 profiles the device's MediaCodec allowlist
      // actually reports as supported -- not every device does.
      for (const auto& format : hw_->GetSupportedFormats()) {
        if (IsH264(format)) formats.push_back(format);
      }
    }
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
//
// Uses webrtc::GetClass() rather than JNIEnv::FindClass() directly: this
// constructor has no requirement to run on a Java-originated thread (it's
// called from reactor_webrtc_factory_create_with_android_hw_h264 after
// AttachCurrentThreadIfNeeded), and on a thread attached from native code
// FindClass only consults the bootstrap class loader, which cannot resolve
// app-provided libwebrtc.jar classes. GetClass() goes through the app class
// loader webrtc::InitClassLoader() cached at reactor_webrtc_android_init()
// time instead.
jobject NewEncoderFactory(JNIEnv* jni) {
  webrtc::ScopedJavaLocalRef<jclass> klass =
      webrtc::GetClass(jni, kHwEncoderFactoryClass);
  if (ClearPendingException(jni, "GetClass(HardwareVideoEncoderFactory)") ||
      !klass.obj()) {
    return nullptr;
  }
  std::string sig = "(L" + std::string(kEglBaseContextClass) + ";ZZ)V";
  jmethodID ctor = jni->GetMethodID(klass.obj(), "<init>", sig.c_str());
  if (ClearPendingException(jni, "GetMethodID(HardwareVideoEncoderFactory.<init>)") ||
      !ctor) {
    return nullptr;
  }
  jobject obj = jni->NewObject(klass.obj(), ctor, /*sharedContext=*/nullptr,
                                /*enableIntelVp8Encoder=*/JNI_FALSE,
                                /*enableH264HighProfile=*/JNI_FALSE);
  ClearPendingException(jni, "NewObject(HardwareVideoEncoderFactory)");
  return obj;
}

// Same idea for the decoder, which has a simpler single-arg ctor
// (EglBase.Context) -- null again for the same reason.
jobject NewDecoderFactory(JNIEnv* jni) {
  webrtc::ScopedJavaLocalRef<jclass> klass =
      webrtc::GetClass(jni, kHwDecoderFactoryClass);
  if (ClearPendingException(jni, "GetClass(HardwareVideoDecoderFactory)") ||
      !klass.obj()) {
    return nullptr;
  }
  std::string sig = "(L" + std::string(kEglBaseContextClass) + ";)V";
  jmethodID ctor = jni->GetMethodID(klass.obj(), "<init>", sig.c_str());
  if (ClearPendingException(jni, "GetMethodID(HardwareVideoDecoderFactory.<init>)") ||
      !ctor) {
    return nullptr;
  }
  jobject obj = jni->NewObject(klass.obj(), ctor, /*sharedContext=*/nullptr);
  ClearPendingException(jni, "NewObject(HardwareVideoDecoderFactory)");
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
