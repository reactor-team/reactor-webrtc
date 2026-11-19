// A decoded picture whose packed R,G,B bytes a decoder already made (NVDEC
// converts on the GPU), so the media path's frame sink and the standalone
// decoder hand it out without converting it again.
#pragma once

#include <cstdint>
#include <functional>
#include <mutex>
#include <unordered_set>
#include <utility>

#include "api/scoped_refptr.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame_buffer.h"
#include "third_party/libyuv/include/libyuv/convert.h"

namespace reactor {

// kNative, so libwebrtc passes it along untouched; a consumer that needs
// planes gets I420 from ToI420, converted on the CPU. The pixels belong to
// whoever gave them: `release` runs once the last reference is gone.
class RgbFrameBuffer : public webrtc::VideoFrameBuffer {
 public:
  RgbFrameBuffer(int width, int height, const uint8_t* rgb, int stride,
                 std::function<void()> release)
      : width_(width),
        height_(height),
        rgb_(rgb),
        stride_(stride),
        release_(std::move(release)) {
    std::lock_guard<std::mutex> lock(LiveMutex());
    Live().insert(this);
  }
  ~RgbFrameBuffer() override {
    {
      std::lock_guard<std::mutex> lock(LiveMutex());
      Live().erase(this);
    }
    if (release_) release_();
  }

  Type type() const override { return Type::kNative; }
  int width() const override { return width_; }
  int height() const override { return height_; }
  webrtc::scoped_refptr<webrtc::I420BufferInterface> ToI420() override {
    webrtc::scoped_refptr<webrtc::I420Buffer> i420 =
        webrtc::I420Buffer::Create(width_, height_);
    libyuv::RAWToI420(rgb_, stride_, i420->MutableDataY(), i420->StrideY(),
                      i420->MutableDataU(), i420->StrideU(),
                      i420->MutableDataV(), i420->StrideV(), width_, height_);
    return i420;
  }

  // R,G,B bytes, `stride` apart from one row to the next.
  const uint8_t* rgb() const { return rgb_; }
  int stride() const { return stride_; }

  // `buffer` as an RgbFrameBuffer, or null when it is another kind. libwebrtc
  // is built without RTTI, so the live instances are tracked instead of
  // cast-checked.
  static const RgbFrameBuffer* From(const webrtc::VideoFrameBuffer* buffer) {
    if (!buffer || buffer->type() != Type::kNative) return nullptr;
    std::lock_guard<std::mutex> lock(LiveMutex());
    return Live().count(buffer) ? static_cast<const RgbFrameBuffer*>(buffer)
                                : nullptr;
  }

 private:
  static std::mutex& LiveMutex() {
    static std::mutex* mutex = new std::mutex();
    return *mutex;
  }
  // Never destroyed, so a buffer released during static destruction still
  // finds it.
  static std::unordered_set<const void*>& Live() {
    static auto* live = new std::unordered_set<const void*>();
    return *live;
  }

  const int width_;
  const int height_;
  const uint8_t* const rgb_;
  const int stride_;
  std::function<void()> release_;
};

}  // namespace reactor
