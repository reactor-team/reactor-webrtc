// Experiment (not for merge): one place that turns a decoded frame's buffer
// into the packed pixels a consumer asked for, shared by the media path's
// frame sink and the standalone decoder.
//
// REACTOR_EXP_NV12_DIRECT=1 converts an NV12 buffer straight to RGB24/BGRA
// instead of through I420. An RgbFrameBuffer (RGB a decoder already made, on
// the GPU with REACTOR_EXP_GPU_RGB=1) is copied as is.
#pragma once

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <mutex>
#include <unordered_set>

#include "api/scoped_refptr.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame_buffer.h"
#include "decode_timing.h"
#include "third_party/libyuv/include/libyuv/convert.h"
#include "third_party/libyuv/include/libyuv/convert_argb.h"
#include "third_party/libyuv/include/libyuv/convert_from_argb.h"

namespace reactor_frame_pixels {

// The pixel layouts a raw video frame crosses the C boundary in (the same
// values as reactor_webrtc.cpp's kPixelFormat*).
constexpr int kBgra = 0;
constexpr int kRgb24 = 1;

inline bool EnvOn(const char* name) {
  const char* v = std::getenv(name);
  return v && *v && std::strcmp(v, "0") != 0;
}

inline bool Nv12Direct() {
  static const bool on = EnvOn("REACTOR_EXP_NV12_DIRECT");
  return on;
}

// A frame whose packed R,G,B bytes a decoder already produced. kNative, so
// libwebrtc leaves it alone; whoever needs planes gets I420 from ToI420. The
// pixels belong to `release`, called when the last reference goes.
class RgbFrameBuffer : public webrtc::VideoFrameBuffer {
 public:
  RgbFrameBuffer(int width, int height, const uint8_t* rgb, int stride,
                 std::function<void()> release)
      : width_(width),
        height_(height),
        rgb_(rgb),
        stride_(stride),
        release_(std::move(release)) {
    std::lock_guard<std::mutex> lock(RegistryMutex());
    Registry().insert(this);
  }
  ~RgbFrameBuffer() override {
    {
      std::lock_guard<std::mutex> lock(RegistryMutex());
      Registry().erase(this);
    }
    if (release_) release_();
  }

  Type type() const override { return Type::kNative; }
  int width() const override { return width_; }
  int height() const override { return height_; }
  webrtc::scoped_refptr<webrtc::I420BufferInterface> ToI420() override {
    DECODE_TIMING("rgb.to_i420_fallback");
    auto i420 = webrtc::I420Buffer::Create(width_, height_);
    libyuv::RAWToI420(rgb_, stride_, i420->MutableDataY(), i420->StrideY(),
                      i420->MutableDataU(), i420->StrideU(),
                      i420->MutableDataV(), i420->StrideV(), width_, height_);
    return i420;
  }

  const uint8_t* rgb() const { return rgb_; }
  int stride() const { return stride_; }

  // `buffer` as an RgbFrameBuffer, or null. The glue builds without RTTI, so
  // live instances are tracked instead of cast-checked.
  static const RgbFrameBuffer* From(const webrtc::VideoFrameBuffer* buffer) {
    if (!buffer || buffer->type() != Type::kNative) return nullptr;
    std::lock_guard<std::mutex> lock(RegistryMutex());
    return Registry().count(buffer) ? static_cast<const RgbFrameBuffer*>(buffer)
                                    : nullptr;
  }

 private:
  static std::mutex& RegistryMutex() {
    static std::mutex m;
    return m;
  }
  static std::unordered_set<const void*>& Registry() {
    static auto* set = new std::unordered_set<const void*>();
    return *set;
  }

  const int width_;
  const int height_;
  const uint8_t* const rgb_;
  const int stride_;
  std::function<void()> release_;
};

// Writes `buffer`'s picture to `dst` as packed `pixel_format` pixels (width*3
// or width*4 bytes per row, no padding). False when it cannot.
inline bool FramePixels(webrtc::VideoFrameBuffer* buffer, int pixel_format,
                        uint8_t* dst) {
  const int w = buffer->width(), h = buffer->height();
  const int dst_stride = w * (pixel_format == kRgb24 ? 3 : 4);
  if (const RgbFrameBuffer* rgb = RgbFrameBuffer::From(buffer)) {
    if (pixel_format == kRgb24) {
      DECODE_TIMING("pixels.rgb_copy");
      if (rgb->stride() == dst_stride) {
        std::memcpy(dst, rgb->rgb(), static_cast<size_t>(dst_stride) * h);
      } else {
        for (int y = 0; y < h; ++y)
          std::memcpy(dst + static_cast<size_t>(y) * dst_stride,
                      rgb->rgb() + static_cast<size_t>(y) * rgb->stride(),
                      dst_stride);
      }
    } else {
      DECODE_TIMING("pixels.rgb_to_bgra");
      libyuv::RAWToARGB(rgb->rgb(), rgb->stride(), dst, dst_stride, w, h);
    }
    return true;
  }
  if (Nv12Direct() && buffer->type() == webrtc::VideoFrameBuffer::Type::kNV12) {
    const webrtc::NV12BufferInterface* nv12 = buffer->GetNV12();
    if (pixel_format == kRgb24) {
      DECODE_TIMING("pixels.nv12_to_rgb");
      libyuv::NV12ToRAW(nv12->DataY(), nv12->StrideY(), nv12->DataUV(),
                        nv12->StrideUV(), dst, dst_stride, w, h);
    } else {
      DECODE_TIMING("pixels.nv12_to_bgra");
      libyuv::NV12ToARGB(nv12->DataY(), nv12->StrideY(), nv12->DataUV(),
                         nv12->StrideUV(), dst, dst_stride, w, h);
    }
    return true;
  }
  webrtc::scoped_refptr<webrtc::I420BufferInterface> i420;
  {
    DECODE_TIMING("pixels.to_i420");
    i420 = buffer->ToI420();
  }
  if (!i420) return false;
  if (pixel_format == kRgb24) {
    DECODE_TIMING("pixels.i420_to_rgb");
    libyuv::I420ToRAW(i420->DataY(), i420->StrideY(), i420->DataU(),
                      i420->StrideU(), i420->DataV(), i420->StrideV(), dst,
                      dst_stride, w, h);
  } else {
    DECODE_TIMING("pixels.i420_to_bgra");
    libyuv::I420ToARGB(i420->DataY(), i420->StrideY(), i420->DataU(),
                       i420->StrideU(), i420->DataV(), i420->StrideV(), dst,
                       dst_stride, w, h);
  }
  return true;
}

}  // namespace reactor_frame_pixels
