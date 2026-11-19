#include "nvdec_codec.h"

#include <algorithm>
#include <cstring>
#include <utility>
#include <vector>

#include "api/video/encoded_image.h"
#include "api/video/nv12_buffer.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_codec.h"
#include "api/video_codecs/video_decoder.h"
#include "common_video/include/video_frame_buffer_pool.h"
#include "modules/video_coding/include/video_error_codes.h"

// The loader reports a library or symbol it cannot load on stderr by default.
// A machine without an NVIDIA driver is an expected case here, not an error,
// so its messages are dropped; OpenNvdec reports the reason instead.
#define FFNV_LOG_FUNC(logctx, msg, ...) ((void)0)
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...) ((void)0)
#include "vendor/dynlink_loader.h"
#include "../decode_timing.h"
#include "../frame_pixels.h"
#include "nv12_to_rgb_ptx.h"

#include <memory>
#include <mutex>

#include "api/make_ref_counted.h"

#include "third_party/libyuv/include/libyuv/convert.h"

namespace reactor {

class NvdecLibrary {
 public:
  ~NvdecLibrary() {
    if (cu && context) cu->cuDevicePrimaryCtxRelease(device);
    cuvid_free_functions(&cuvid);
    cuda_free_functions(&cu);
  }

  CudaFunctions* cu = nullptr;
  CuvidFunctions* cuvid = nullptr;
  CUdevice device = 0;
  CUcontext context = nullptr;
  // Experiment: page-locked host memory (not in the vendored loader, so
  // looked up in the same libcuda) and the NV12 to RGB kernel.
  CUresult (*cuMemAllocHost)(void**, size_t) = nullptr;
  CUresult (*cuMemFreeHost)(void*) = nullptr;
  CUmodule rgb_module = nullptr;
  CUfunction nv12_to_rgb = nullptr;
  // Whether the GPU decodes a codec at 8-bit 4:2:0, and the coded sizes it
  // decodes it at, asked once at open.
  struct Caps {
    bool decodes = false;
    unsigned int min_width = 0;
    unsigned int min_height = 0;
    unsigned int max_width = 0;
    unsigned int max_height = 0;
    unsigned int max_mb_count = 0;
  };
  Caps h264;
  Caps vp8;
  Caps vp9;
  Caps av1;
};

namespace {

// Makes the library's context current on this thread for the scope. Every
// nvcuvid call needs it, and libwebrtc decodes on a thread of its own.
class ScopedContext {
 public:
  explicit ScopedContext(const NvdecLibrary& lib) : lib_(lib) {
    pushed_ = lib_.cu->cuCtxPushCurrent(lib_.context) == CUDA_SUCCESS;
  }
  ~ScopedContext() {
    if (pushed_) {
      CUcontext popped = nullptr;
      lib_.cu->cuCtxPopCurrent(&popped);
    }
  }
  bool ok() const { return pushed_; }

 private:
  const NvdecLibrary& lib_;
  bool pushed_ = false;
};

// Experiment: page-locked host blocks of one size, reused once the frame
// holding one is dropped. Outlives its decoder while frames are out.
class PinnedPool : public std::enable_shared_from_this<PinnedPool> {
 public:
  PinnedPool(std::shared_ptr<NvdecLibrary> lib, size_t block_size)
      : lib_(std::move(lib)), block_size_(block_size) {}
  ~PinnedPool() {
    ScopedContext scope(*lib_);
    for (void* b : free_) lib_->cuMemFreeHost(b);
  }
  size_t block_size() const { return block_size_; }
  // A block, or null when page-locked memory is unavailable.
  void* Take() {
    {
      std::lock_guard<std::mutex> lock(mu_);
      if (!free_.empty()) {
        void* b = free_.back();
        free_.pop_back();
        return b;
      }
    }
    if (!lib_->cuMemAllocHost) return nullptr;
    void* b = nullptr;
    DECODE_TIMING("nvdec.pinned_alloc");
    if (lib_->cuMemAllocHost(&b, block_size_) != CUDA_SUCCESS) return nullptr;
    return b;
  }
  void Give(void* b) {
    std::lock_guard<std::mutex> lock(mu_);
    free_.push_back(b);
  }

 private:
  std::shared_ptr<NvdecLibrary> lib_;
  const size_t block_size_;
  std::mutex mu_;
  std::vector<void*> free_;
};

// Experiment: an NV12 picture in a page-locked block of a PinnedPool.
class PinnedNV12Buffer : public webrtc::NV12BufferInterface {
 public:
  static webrtc::scoped_refptr<PinnedNV12Buffer> Create(
      std::shared_ptr<PinnedPool> pool, int width, int height) {
    void* block = pool->Take();
    if (!block) return nullptr;
    return webrtc::make_ref_counted<PinnedNV12Buffer>(std::move(pool), block,
                                                      width, height);
  }
  PinnedNV12Buffer(std::shared_ptr<PinnedPool> pool, void* block, int width,
                   int height)
      : pool_(std::move(pool)),
        block_(static_cast<uint8_t*>(block)),
        width_(width),
        height_(height) {}
  ~PinnedNV12Buffer() override { pool_->Give(block_); }

  static size_t BytesFor(int width, int height) {
    const size_t stride = static_cast<size_t>((width + 1) & ~1);
    return stride * height + stride * ((height + 1) / 2);
  }

  int width() const override { return width_; }
  int height() const override { return height_; }
  int StrideY() const override { return (width_ + 1) & ~1; }
  int StrideUV() const override { return (width_ + 1) & ~1; }
  const uint8_t* DataY() const override { return block_; }
  const uint8_t* DataUV() const override {
    return block_ + static_cast<size_t>(StrideY()) * height_;
  }
  uint8_t* MutableDataY() { return block_; }
  uint8_t* MutableDataUV() {
    return block_ + static_cast<size_t>(StrideY()) * height_;
  }
  webrtc::scoped_refptr<webrtc::I420BufferInterface> ToI420() override {
    auto i420 = webrtc::I420Buffer::Create(width_, height_);
    libyuv::NV12ToI420(DataY(), StrideY(), DataUV(), StrideUV(),
                       i420->MutableDataY(), i420->StrideY(),
                       i420->MutableDataU(), i420->StrideU(),
                       i420->MutableDataV(), i420->StrideV(), width_, height_);
    return i420;
  }

 private:
  std::shared_ptr<PinnedPool> pool_;
  uint8_t* const block_;
  const int width_;
  const int height_;
};

void SetWhy(std::string* why, std::string reason) {
  if (why) *why = std::move(reason);
}

// The nvcuvid codec of a libwebrtc one; cudaVideoCodec_NumCodecs for one
// NVDEC is not used for.
cudaVideoCodec CuvidCodec(webrtc::VideoCodecType codec) {
  switch (codec) {
    case webrtc::kVideoCodecH264:
      return cudaVideoCodec_H264;
    case webrtc::kVideoCodecVP8:
      return cudaVideoCodec_VP8;
    case webrtc::kVideoCodecVP9:
      return cudaVideoCodec_VP9;
    case webrtc::kVideoCodecAV1:
      return cudaVideoCodec_AV1;
    default:
      return cudaVideoCodec_NumCodecs;
  }
}

// The formats NVDEC decodes on `lib`'s GPU. H.264 in the profiles it decodes,
// at level 3.1, the level libwebrtc lists its own H.264 at (a decoder accepts
// any level its profile allows); VP9 profile 0 and AV1 main, the 8-bit 4:2:0
// ones.
std::vector<webrtc::SdpVideoFormat> NvdecFormats(const NvdecLibrary& lib) {
  std::vector<webrtc::SdpVideoFormat> formats;
  if (lib.h264.decodes) {
    for (const char* profile_level_id : {"42e01f",   // constrained baseline
                                         "42001f",   // baseline
                                         "4d001f",   // main
                                         "64001f"}) {  // high
      formats.emplace_back(
          "H264", webrtc::CodecParameterMap{{"level-asymmetry-allowed", "1"},
                                            {"packetization-mode", "1"},
                                            {"profile-level-id", profile_level_id}});
    }
  }
  if (lib.vp8.decodes) formats.push_back(webrtc::SdpVideoFormat::VP8());
  if (lib.vp9.decodes) formats.push_back(webrtc::SdpVideoFormat::VP9Profile0());
  if (lib.av1.decodes) formats.push_back(webrtc::SdpVideoFormat::AV1Profile0());
  return formats;
}

// Whether `lib`'s GPU decodes `codec` at 8-bit 4:2:0, and at which coded
// sizes. Its context must be current. A driver without cuvidGetDecoderCaps
// (the loader leaves it null) decodes nothing here.
NvdecLibrary::Caps AskDecodes(const NvdecLibrary& lib, cudaVideoCodec codec) {
  NvdecLibrary::Caps out;
  if (!lib.cuvid->cuvidGetDecoderCaps) return out;
  CUVIDDECODECAPS caps;
  memset(&caps, 0, sizeof(caps));
  caps.eCodecType = codec;
  caps.eChromaFormat = cudaVideoChromaFormat_420;
  caps.nBitDepthMinus8 = 0;
  if (lib.cuvid->cuvidGetDecoderCaps(&caps) != CUDA_SUCCESS ||
      !caps.bIsSupported) {
    return out;
  }
  out.decodes = true;
  out.min_width = caps.nMinWidth;
  out.min_height = caps.nMinHeight;
  out.max_width = caps.nMaxWidth;
  out.max_height = caps.nMaxHeight;
  out.max_mb_count = caps.nMaxMBCount;
  return out;
}

const NvdecLibrary::Caps& CapsOf(const NvdecLibrary& lib,
                                 cudaVideoCodec codec) {
  static const NvdecLibrary::Caps kNone{};
  switch (codec) {
    case cudaVideoCodec_H264:
      return lib.h264;
    case cudaVideoCodec_VP8:
      return lib.vp8;
    case cudaVideoCodec_VP9:
      return lib.vp9;
    case cudaVideoCodec_AV1:
      return lib.av1;
    default:
      return kNone;
  }
}

// One stream of one codec. libwebrtc hands Decode() one frame at a time (an
// H.264 access unit, a VP8 or VP9 frame, an AV1 temporal unit); with no
// display delay the parser decodes it and maps the picture before
// cuvidParseVideoData returns, so a frame comes out of the Decode() call that
// put it in, as the software decoders do. A frame that is not shown (a hidden
// VP9 or AV1 frame) gives no picture. The picture is copied from the GPU as
// NV12; consumers that want I420 or RGB convert from there. A stream the GPU
// does not decode (high bit depth, 4:2:2 or 4:4:4, a coded size outside its
// limits) or a decoder it cannot create returns
// WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE, for a software decoder to take over.
class NvdecVideoDecoder : public webrtc::VideoDecoder {
  // Pictures handed out and not yet dropped, at most; libvpx's decoders allow
  // as many.
  static constexpr size_t kMaxBuffers = 300;

 public:
  NvdecVideoDecoder(std::shared_ptr<NvdecLibrary> lib, cudaVideoCodec codec)
      : lib_(std::move(lib)), codec_(codec), caps_(CapsOf(*lib_, codec)) {}

  ~NvdecVideoDecoder() override { Release(); }

  bool Configure(const Settings&) override {
    Release();
    ScopedContext scope(*lib_);
    if (!scope.ok()) return false;
    if (lib_->cuvid->cuvidCtxLockCreate(&lock_, lib_->context) != CUDA_SUCCESS) {
      lock_ = nullptr;
      return false;
    }
    return CreateParser();
  }

  int32_t RegisterDecodeCompleteCallback(
      webrtc::DecodedImageCallback* cb) override {
    callback_ = cb;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Release() override {
    if (!lib_) return WEBRTC_VIDEO_CODEC_OK;
    ScopedContext scope(*lib_);
    DestroyParserAndDecoder();
    if (lock_) {
      lib_->cuvid->cuvidCtxLockDestroy(lock_);
      lock_ = nullptr;
    }
    buffer_pool_.Release();
    if (rgb_device_) {
      lib_->cu->cuMemFree(rgb_device_);
      rgb_device_ = 0;
      rgb_bytes_ = 0;
    }
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Decode(const webrtc::EncodedImage& input,
                 int64_t /*render_time_ms*/) override {
    if (!parser_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    if (input.size() == 0) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    DECODE_TIMING("nvdec.Decode_total");
    ScopedContext scope(*lib_);
    if (!scope.ok()) return WEBRTC_VIDEO_CODEC_ERROR;

    CUVIDSOURCEDATAPACKET packet;
    memset(&packet, 0, sizeof(packet));
    packet.payload = input.data();
    packet.payload_size = static_cast<unsigned long>(input.size());
    // One whole frame: the parser decodes it now rather than waiting for the
    // start of the next one to know it ended.
    packet.flags = CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE;
    packet.timestamp = input.RtpTimestamp();

    failed_ = false;
    fallback_ = false;
    produced_ = false;
    CUresult parsed;
    {
      DECODE_TIMING("nvdec.parse_total");
      parsed = lib_->cuvid->cuvidParseVideoData(parser_, &packet);
    }
    if (parsed != CUDA_SUCCESS || failed_ || fallback_) {
      // Start over at the next key frame: a reference lost here would corrupt
      // every frame until then. libwebrtc asks the sender for one on error.
      DestroyParserAndDecoder();
      CreateParser();
      // A stream NVDEC does not decode goes to software, from this frame on
      // (the sequence that set it starts at a key frame).
      return fallback_ ? WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE
                       : WEBRTC_VIDEO_CODEC_ERROR;
    }
    return produced_ ? WEBRTC_VIDEO_CODEC_OK : WEBRTC_VIDEO_CODEC_NO_OUTPUT;
  }

  DecoderInfo GetDecoderInfo() const override {
    return {"NVDEC", /*is_hardware_accelerated=*/true};
  }

 private:
  static int CUDAAPI OnSequence(void* self, CUVIDEOFORMAT* format) {
    return static_cast<NvdecVideoDecoder*>(self)->HandleSequence(format);
  }
  static int CUDAAPI OnDecode(void* self, CUVIDPICPARAMS* picture) {
    return static_cast<NvdecVideoDecoder*>(self)->HandleDecode(picture);
  }
  static int CUDAAPI OnDisplay(void* self, CUVIDPARSERDISPINFO* display) {
    return static_cast<NvdecVideoDecoder*>(self)->HandleDisplay(display);
  }

  bool CreateParser() {
    CUVIDPARSERPARAMS params;
    memset(&params, 0, sizeof(params));
    params.CodecType = codec_;
    // Raised by the sequence callback's return value once the stream says how
    // many it needs.
    params.ulMaxNumDecodeSurfaces = 1;
    params.ulClockRate = 90000;
    params.ulMaxDisplayDelay = 0;
    params.pUserData = this;
    params.pfnSequenceCallback = &OnSequence;
    params.pfnDecodePicture = &OnDecode;
    params.pfnDisplayPicture = &OnDisplay;
    if (lib_->cuvid->cuvidCreateVideoParser(&parser_, &params) != CUDA_SUCCESS) {
      parser_ = nullptr;
      return false;
    }
    return true;
  }

  void DestroyParserAndDecoder() {
    if (parser_) {
      lib_->cuvid->cuvidDestroyVideoParser(parser_);
      parser_ = nullptr;
    }
    if (decoder_) {
      lib_->cuvid->cuvidDestroyDecoder(decoder_);
      decoder_ = nullptr;
    }
    coded_width_ = coded_height_ = 0;
  }

  // Whether the GPU decodes `format`'s coded size.
  bool FitsCaps(const CUVIDEOFORMAT& format) const {
    const unsigned int w = format.coded_width;
    const unsigned int h = format.coded_height;
    return w >= caps_.min_width && h >= caps_.min_height &&
           w <= caps_.max_width && h <= caps_.max_height &&
           (static_cast<unsigned long long>(w) * h) / 256 <= caps_.max_mb_count;
  }

  // A new sequence, or a change of format: (re)creates the decoder for it.
  // Returns the number of decode surfaces the parser should cycle through,
  // 0 to fail.
  int HandleSequence(CUVIDEOFORMAT* format) {
    if (format->codec != codec_ ||
        format->chroma_format != cudaVideoChromaFormat_420 ||
        format->bit_depth_luma_minus8 != 0 || !FitsCaps(*format)) {
      // 4:2:2, 4:4:4, high bit depth (not 8-bit NV12) or a size the GPU does
      // not decode: software does.
      fallback_ = true;
      return 0;
    }
    const int surfaces = format->min_num_decode_surfaces > 0
                             ? format->min_num_decode_surfaces
                             : 4;
    const int width = format->display_area.right - format->display_area.left;
    const int height = format->display_area.bottom - format->display_area.top;
    if (decoder_ && coded_width_ == format->coded_width &&
        coded_height_ == format->coded_height && width_ == width &&
        height_ == height && surfaces_ >= surfaces) {
      return surfaces_;
    }
    if (decoder_) {
      lib_->cuvid->cuvidDestroyDecoder(decoder_);
      decoder_ = nullptr;
    }

    CUVIDDECODECREATEINFO info;
    memset(&info, 0, sizeof(info));
    info.ulWidth = format->coded_width;
    info.ulHeight = format->coded_height;
    info.ulMaxWidth = format->coded_width;
    info.ulMaxHeight = format->coded_height;
    info.ulNumDecodeSurfaces = surfaces;
    info.CodecType = codec_;
    info.ChromaFormat = cudaVideoChromaFormat_420;
    info.bitDepthMinus8 = 0;
    info.ulCreationFlags = cudaVideoCreate_PreferCUVID;
    info.OutputFormat = cudaVideoSurfaceFormat_NV12;
    info.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
    // The output is the display area, cropped by the decoder. NV12 needs even
    // dimensions: an odd display area is widened by a column or row into the
    // coded size's padding, so nothing is rescaled, and Copy leaves it out.
    const int target_width = (width + 1) & ~1;
    const int target_height = (height + 1) & ~1;
    info.display_area.left = static_cast<short>(format->display_area.left);
    info.display_area.top = static_cast<short>(format->display_area.top);
    info.display_area.right = static_cast<short>(
        std::min<int>(format->display_area.left + target_width,
                      static_cast<int>(format->coded_width)));
    info.display_area.bottom = static_cast<short>(
        std::min<int>(format->display_area.top + target_height,
                      static_cast<int>(format->coded_height)));
    info.ulTargetWidth = target_width;
    info.ulTargetHeight = target_height;
    info.ulNumOutputSurfaces = 1;
    info.vidLock = lock_;
    if (lib_->cuvid->cuvidCreateDecoder(&decoder_, &info) != CUDA_SUCCESS) {
      // Out of GPU memory or decode sessions, say: software decodes it.
      decoder_ = nullptr;
      fallback_ = true;
      return 0;
    }
    coded_width_ = format->coded_width;
    coded_height_ = format->coded_height;
    width_ = width;
    height_ = height;
    surface_height_ = static_cast<int>(info.ulTargetHeight);
    surfaces_ = surfaces;
    return surfaces;
  }

  int HandleDecode(CUVIDPICPARAMS* picture) {
    DECODE_TIMING("nvdec.decode_picture");
    if (!decoder_ ||
        lib_->cuvid->cuvidDecodePicture(decoder_, picture) != CUDA_SUCCESS) {
      failed_ = true;
      return 0;
    }
    return 1;
  }

  int HandleDisplay(CUVIDPARSERDISPINFO* display) {
    if (!display) return 1;  // End of stream: nothing to show.
    if (!decoder_) {
      failed_ = true;
      return 0;
    }
    DECODE_TIMING("nvdec.display_total");
    CUVIDGETDECODESTATUS status;
    memset(&status, 0, sizeof(status));
    if (lib_->cuvid->cuvidGetDecodeStatus &&
        lib_->cuvid->cuvidGetDecodeStatus(decoder_, display->picture_index,
                                          &status) == CUDA_SUCCESS &&
        (status.decodeStatus == cuvidDecodeStatus_Error ||
         status.decodeStatus == cuvidDecodeStatus_Error_Concealed)) {
      failed_ = true;
      return 0;
    }

    CUVIDPROCPARAMS proc;
    memset(&proc, 0, sizeof(proc));
    proc.progressive_frame = display->progressive_frame;
    proc.top_field_first = display->top_field_first;
    proc.second_field = display->repeat_first_field + 1;
    CUdeviceptr frame = 0;
    unsigned int pitch = 0;
    CUresult mapped;
    {
      DECODE_TIMING("nvdec.map");
      mapped = lib_->cuvid->cuvidMapVideoFrame(decoder_, display->picture_index,
                                               &frame, &pitch, &proc);
    }
    if (mapped != CUDA_SUCCESS) {
      failed_ = true;
      return 0;
    }
    webrtc::scoped_refptr<webrtc::VideoFrameBuffer> out;
    bool copied = false;
    if (lib_->nv12_to_rgb) {
      out = ToRgb(frame, pitch);
      copied = out != nullptr;
    } else if (reactor_frame_pixels::EnvOn("REACTOR_EXP_PINNED") &&
               lib_->cuMemAllocHost) {
      webrtc::scoped_refptr<PinnedNV12Buffer> pinned;
      {
        DECODE_TIMING("nvdec.pool_alloc");
        pinned = PinnedNV12Buffer::Create(
            PoolFor(nv12_pool_, PinnedNV12Buffer::BytesFor(width_, height_)),
            width_, height_);
      }
      if (pinned) {
        DECODE_TIMING("nvdec.copy_total");
        copied = Copy(frame, pitch, pinned->MutableDataY(), pinned->StrideY(),
                      pinned->MutableDataUV(), pinned->StrideUV());
      }
      out = pinned;
    } else {
      webrtc::scoped_refptr<webrtc::NV12Buffer> nv12;
      {
        DECODE_TIMING("nvdec.pool_alloc");
        nv12 = buffer_pool_.CreateNV12Buffer(width_, height_);
      }
      if (nv12) {
        DECODE_TIMING("nvdec.copy_total");
        copied = Copy(frame, pitch, nv12->MutableDataY(), nv12->StrideY(),
                      nv12->MutableDataUV(), nv12->StrideUV());
      }
      out = nv12;
    }
    {
      DECODE_TIMING("nvdec.unmap");
      lib_->cuvid->cuvidUnmapVideoFrame(decoder_, frame);
    }
    if (!copied) {
      failed_ = true;
      return 0;
    }

    if (callback_) {
      webrtc::VideoFrame decoded =
          webrtc::VideoFrame::Builder()
              .set_video_frame_buffer(out)
              .set_rtp_timestamp(static_cast<uint32_t>(display->timestamp))
              .set_timestamp_us(0)
              .build();
      DECODE_TIMING("nvdec.callback");
      callback_->Decoded(decoded);
    }
    produced_ = true;
    return 1;
  }

  // The pool of `slot` when its blocks hold `bytes`, made anew otherwise.
  std::shared_ptr<PinnedPool> PoolFor(std::shared_ptr<PinnedPool>& slot,
                                      size_t bytes) {
    if (!slot || slot->block_size() != bytes)
      slot = std::make_shared<PinnedPool>(lib_, bytes);
    return slot;
  }

  // Experiment: the mapped surface converted to RGB on the GPU and copied to
  // a page-locked block (pageable memory when none is available).
  webrtc::scoped_refptr<webrtc::VideoFrameBuffer> ToRgb(CUdeviceptr frame,
                                                        unsigned int pitch) {
    const size_t rgb_stride = static_cast<size_t>(width_) * 3;
    const size_t bytes = rgb_stride * height_;
    if (rgb_bytes_ != bytes) {
      if (rgb_device_) lib_->cu->cuMemFree(rgb_device_);
      rgb_device_ = 0;
      rgb_bytes_ = 0;
      if (lib_->cu->cuMemAlloc(&rgb_device_, bytes) != CUDA_SUCCESS) {
        rgb_device_ = 0;
        return nullptr;
      }
      rgb_bytes_ = bytes;
    }
    {
      DECODE_TIMING("nvdec.rgb_kernel");
      CUdeviceptr uv = frame + static_cast<CUdeviceptr>(pitch) * surface_height_;
      unsigned int y_pitch = pitch, uv_pitch = pitch,
                   out_pitch = static_cast<unsigned int>(rgb_stride);
      int w = width_, h = height_;
      void* args[] = {&frame, &y_pitch, &uv, &uv_pitch, &rgb_device_,
                      &out_pitch, &w, &h};
      const unsigned int bx = 32, by = 8;
      if (lib_->cu->cuLaunchKernel(lib_->nv12_to_rgb, (w + bx - 1) / bx,
                                   (h + by - 1) / by, 1, bx, by, 1, 0, nullptr,
                                   args, nullptr) != CUDA_SUCCESS) {
        return nullptr;
      }
    }
    std::shared_ptr<PinnedPool> pool = PoolFor(rgb_pool_, bytes);
    void* block = pool->Take();
    std::function<void()> release;
    if (block) {
      release = [pool, block] { pool->Give(block); };
    } else {
      block = std::malloc(bytes);
      if (!block) return nullptr;
      release = [block] { std::free(block); };
    }
    {
      // On the default stream, so it starts once the kernel is done.
      DECODE_TIMING("nvdec.rgb_copy_d2h");
      if (lib_->cu->cuMemcpyDtoH(block, rgb_device_, bytes) != CUDA_SUCCESS) {
        release();
        return nullptr;
      }
    }
    return webrtc::make_ref_counted<reactor_frame_pixels::RgbFrameBuffer>(
        width_, height_, static_cast<const uint8_t*>(block),
        static_cast<int>(rgb_stride), std::move(release));
  }

  // The mapped surface's luma and interleaved chroma planes into the given
  // host planes.
  bool Copy(CUdeviceptr frame, unsigned int pitch, uint8_t* dst_y,
            int stride_y, uint8_t* dst_uv, int stride_uv) {
    CUDA_MEMCPY2D copy;
    memset(&copy, 0, sizeof(copy));
    copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.srcDevice = frame;
    copy.srcPitch = pitch;
    copy.dstMemoryType = CU_MEMORYTYPE_HOST;
    copy.dstHost = dst_y;
    copy.dstPitch = stride_y;
    copy.WidthInBytes = width_;
    copy.Height = height_;
    {
      DECODE_TIMING("nvdec.copy_y");
      if (lib_->cu->cuMemcpy2D(&copy) != CUDA_SUCCESS) return false;
    }

    // Chroma follows the luma of the whole output surface.
    copy.srcDevice = frame + static_cast<CUdeviceptr>(pitch) * surface_height_;
    copy.dstHost = dst_uv;
    copy.dstPitch = stride_uv;
    copy.WidthInBytes = 2 * ((width_ + 1) / 2);
    copy.Height = (height_ + 1) / 2;
    DECODE_TIMING("nvdec.copy_uv");
    return lib_->cu->cuMemcpy2D(&copy) == CUDA_SUCCESS;
  }

  std::shared_ptr<NvdecLibrary> lib_;
  const cudaVideoCodec codec_;
  const NvdecLibrary::Caps caps_;
  // The decoded pictures' buffers, reused once their consumers drop them.
  webrtc::VideoFrameBufferPool buffer_pool_{/*zero_initialize=*/false,
                                            kMaxBuffers};
  // Experiment: page-locked pools for NV12 (REACTOR_EXP_PINNED) and RGB
  // (REACTOR_EXP_GPU_RGB) pictures, and the kernel's device output.
  std::shared_ptr<PinnedPool> nv12_pool_;
  std::shared_ptr<PinnedPool> rgb_pool_;
  CUdeviceptr rgb_device_ = 0;
  size_t rgb_bytes_ = 0;
  CUvideoctxlock lock_ = nullptr;
  CUvideoparser parser_ = nullptr;
  CUvideodecoder decoder_ = nullptr;
  webrtc::DecodedImageCallback* callback_ = nullptr;
  unsigned int coded_width_ = 0;
  unsigned int coded_height_ = 0;
  int width_ = 0;
  int height_ = 0;
  int surface_height_ = 0;
  int surfaces_ = 0;
  // Set by the callbacks during one cuvidParseVideoData call: `failed_` for an
  // error, `fallback_` for a stream software has to decode.
  bool failed_ = false;
  bool fallback_ = false;
  bool produced_ = false;
};

class NvdecVideoDecoderFactory : public webrtc::VideoDecoderFactory {
 public:
  explicit NvdecVideoDecoderFactory(std::shared_ptr<NvdecLibrary> lib)
      : lib_(std::move(lib)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return NvdecFormats(*lib_);
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& /*env*/,
      const webrtc::SdpVideoFormat& format) override {
    // Only a format listed: VP9 profile 2 stays in software. A stream of a
    // listed format NVDEC does not decode (10-bit AV1, say) falls back to
    // software when its sequence header arrives.
    bool listed = false;
    for (const auto& f : NvdecFormats(*lib_))
      if (f.IsSameCodec(format)) listed = true;
    if (!listed) return nullptr;
    const cudaVideoCodec codec =
        CuvidCodec(webrtc::PayloadStringToCodecType(format.name));
    if (codec == cudaVideoCodec_NumCodecs) return nullptr;
    return std::make_unique<NvdecVideoDecoder>(lib_, codec);
  }

 private:
  std::shared_ptr<NvdecLibrary> lib_;
};

}  // namespace

std::shared_ptr<NvdecLibrary> OpenNvdec(int device, std::string* why) {
  auto lib = std::make_shared<NvdecLibrary>();
  if (cuda_load_functions(&lib->cu, nullptr) < 0) {
    SetWhy(why, "the NVIDIA driver's libcuda.so.1 did not load");
    return nullptr;
  }
  if (cuvid_load_functions(&lib->cuvid, nullptr) < 0) {
    SetWhy(why, "the NVIDIA driver's libnvcuvid.so.1 did not load");
    return nullptr;
  }
  if (lib->cu->cuInit(0) != CUDA_SUCCESS) {
    SetWhy(why, "CUDA did not initialise");
    return nullptr;
  }
  int count = 0;
  if (lib->cu->cuDeviceGetCount(&count) != CUDA_SUCCESS || device < 0 ||
      device >= count) {
    SetWhy(why, "no NVIDIA GPU " + std::to_string(device) + " (" +
                    std::to_string(count) + " visible)");
    return nullptr;
  }
  if (lib->cu->cuDeviceGet(&lib->device, device) != CUDA_SUCCESS ||
      lib->cu->cuDevicePrimaryCtxRetain(&lib->context, lib->device) !=
          CUDA_SUCCESS) {
    lib->context = nullptr;
    SetWhy(why, "could not retain GPU " + std::to_string(device) + "'s context");
    return nullptr;
  }

  {
    ScopedContext scope(*lib);
    if (!scope.ok()) {
      SetWhy(why, "could not make GPU " + std::to_string(device) +
                      "'s context current");
      return nullptr;
    }
    lib->h264 = AskDecodes(*lib, cudaVideoCodec_H264);
    lib->vp8 = AskDecodes(*lib, cudaVideoCodec_VP8);
    lib->vp9 = AskDecodes(*lib, cudaVideoCodec_VP9);
    lib->av1 = AskDecodes(*lib, cudaVideoCodec_AV1);
    lib->cuMemAllocHost = reinterpret_cast<CUresult (*)(void**, size_t)>(
        FFNV_SYM_FUNC(lib->cu->lib, "cuMemAllocHost_v2"));
    lib->cuMemFreeHost = reinterpret_cast<CUresult (*)(void*)>(
        FFNV_SYM_FUNC(lib->cu->lib, "cuMemFreeHost"));
    if (reactor_frame_pixels::EnvOn("REACTOR_EXP_GPU_RGB")) {
      if (lib->cu->cuModuleLoadData(&lib->rgb_module, kNv12ToRgbPtx) !=
              CUDA_SUCCESS ||
          lib->cu->cuModuleGetFunction(&lib->nv12_to_rgb, lib->rgb_module,
                                       "nv12_to_rgb24") != CUDA_SUCCESS) {
        fprintf(stderr, "[nvdec-exp] the NV12 to RGB kernel did not load\n");
        lib->nv12_to_rgb = nullptr;
      }
    }
  }
  if (!lib->h264.decodes && !lib->vp8.decodes && !lib->vp9.decodes &&
      !lib->av1.decodes) {
    SetWhy(why, "GPU " + std::to_string(device) +
                    " decodes none of H.264, VP8, VP9 and AV1");
    return nullptr;
  }
  return lib;
}

bool NvdecDecodes(const NvdecLibrary& lib, webrtc::VideoCodecType codec) {
  switch (codec) {
    case webrtc::kVideoCodecH264:
      return lib.h264.decodes;
    case webrtc::kVideoCodecVP8:
      return lib.vp8.decodes;
    case webrtc::kVideoCodecVP9:
      return lib.vp9.decodes;
    case webrtc::kVideoCodecAV1:
      return lib.av1.decodes;
    default:
      return false;
  }
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateNvdecVideoDecoderFactory(
    std::shared_ptr<NvdecLibrary> lib) {
  return std::make_unique<NvdecVideoDecoderFactory>(std::move(lib));
}

}  // namespace reactor
