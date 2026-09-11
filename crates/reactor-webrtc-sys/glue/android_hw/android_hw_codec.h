// Real H.264 encode/decode on Android via the platform's MediaCodec, bridged
// in through WebRTC's own Java HardwareVideoEncoderFactory/DecoderFactory
// (org.webrtc, namespaced inc.reactor.org.webrtc.* by
// webrtc-build/patches/0002-android-jni-package-prefix.patch) rather than a
// bespoke NDK MediaCodec integration -- reuses WebRTC's own device quirks
// database and codec allow/deny lists instead of re-deriving them.
//
// Unlike the OpenH264 (SW, Linux/Windows) track, this needs no libwebrtc.a
// rebuild: the Java-side MediaCodec factories and their native JNI wrapper
// (sdk/android/native_api/codecs/wrapper.h) are already compiled into the
// Android prebuilt.
#pragma once

#include <jni.h>

#include <memory>

#include "api/video_codecs/video_decoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"

namespace reactor {

// Both construct the real MediaCodec-backed Java factory via JNI (with a null
// EglBase.Context -- I420-buffer encode/decode, not the surface/texture path,
// simpler to integrate and still real hardware underneath) and wrap it
// together with the builtin factory: VP8/VP9/AV1 stay on the builtin
// implementation, only H264 routes through the JNI-bridged hardware factory.
//
// If the Java class can't be found/constructed (e.g. libwebrtc.jar isn't on
// the consuming app's classpath -- see README for how to add it), H.264 is
// simply not advertised in SDP rather than the factory constructor failing
// outright, same degrade-gracefully contract as the OpenH264 track.
std::unique_ptr<webrtc::VideoEncoderFactory> CreateAndroidHwVideoEncoderFactory(
    JNIEnv* jni);
std::unique_ptr<webrtc::VideoDecoderFactory> CreateAndroidHwVideoDecoderFactory(
    JNIEnv* jni);

}  // namespace reactor
