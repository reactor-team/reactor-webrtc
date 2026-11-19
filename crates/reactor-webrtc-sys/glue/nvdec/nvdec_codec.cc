#include "nvdec_codec.h"

#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <utility>
#include <vector>

#include "api/make_ref_counted.h"
#include "api/video/encoded_image.h"
#include "api/video/nv12_buffer.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_codec.h"
#include "api/video_codecs/video_decoder.h"
#include "common_video/include/video_frame_buffer_pool.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"

// The loader reports a library or symbol it cannot load on stderr by default.
// A machine without an NVIDIA driver is an expected case here, not an error,
// so its messages are dropped; OpenNvdec reports the reason instead.
#define FFNV_LOG_FUNC(logctx, msg, ...) ((void)0)
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...) ((void)0)
#include "vendor/dynlink_loader.h"

#include "../rgb_frame_buffer.h"
#include "nv12_to_rgb_ptx.h"

namespace reactor {

class NvdecLibrary {
 public:
  ~NvdecLibrary() {
    if (cu && context) {
      if (rgb_module && cu->cuCtxPushCurrent(context) == CUDA_SUCCESS) {
        cu->cuModuleUnload(rgb_module);
        CUcontext popped = nullptr;
        cu->cuCtxPopCurrent(&popped);
      }
      cu->cuDevicePrimaryCtxRelease(device);
    }
    cuvid_free_functions(&cuvid);
    cuda_free_functions(&cu);
  }

  CudaFunctions* cu = nullptr;
  CuvidFunctions* cuvid = nullptr;
  CUdevice device = 0;
  CUcontext context = nullptr;
  // Page-locked host memory, which the vendored loader does not load; looked
  // up in the same libcuda. Null on a driver without them.
  CUresult (*cuMemAllocHost)(void**, size_t) = nullptr;
  CUresult (*cuMemFreeHost)(void*) = nullptr;
  // The NV12 to RGB kernel (nv12_to_rgb.cu), or null when it did not load:
  // pictures then leave the GPU as NV12 and the CPU converts them.
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

// Page-locked host blocks of one size, for pictures copied off the GPU, reused
// once the frame holding one is dropped. Frames out of a decoder keep its pool
// alive, and it frees its blocks when the last goes. Page-locked memory is
// taken from the whole system, so a pool owns at most kMaxBlocks blocks and
// keeps at most kMaxIdle of them unused.
class PinnedPool {
  // Blocks owned at once, at most (about 100 MiB of 1080p pictures); past it
  // pictures go to ordinary memory.
  static constexpr size_t kMaxBlocks = 16;
  // Unused blocks kept for reuse, at most; the rest are freed as they come
  // back.
  static constexpr size_t kMaxIdle = 4;

 public:
  PinnedPool(std::shared_ptr<NvdecLibrary> lib, size_t block_size)
      : lib_(std::move(lib)), block_size_(block_size) {}
  ~PinnedPool() {
    ScopedContext scope(*lib_);
    for (void* block : free_) lib_->cuMemFreeHost(block);
  }
  size_t block_size() const { return block_size_; }

  // A block, or null when page-locked memory is unavailable or the pool
  // already owns kMaxBlocks.
  void* Take() {
    {
      std::lock_guard<std::mutex> lock(mu_);
      if (!free_.empty()) {
        void* block = free_.back();
        free_.pop_back();
        return block;
      }
      if (owned_ >= kMaxBlocks) return nullptr;
      ++owned_;
    }
    void* block = nullptr;
    if (!lib_->cuMemAllocHost || !lib_->cuMemFreeHost ||
        lib_->cuMemAllocHost(&block, block_size_) != CUDA_SUCCESS) {
      std::lock_guard<std::mutex> lock(mu_);
      --owned_;
      return nullptr;
    }
    return block;
  }
  void Give(void* block) {
    {
      std::lock_guard<std::mutex> lock(mu_);
      if (free_.size() < kMaxIdle) {
        free_.push_back(block);
        return;
      }
      --owned_;
    }
    ScopedContext scope(*lib_);
    lib_->cuMemFreeHost(block);
  }

 private:
  std::shared_ptr<NvdecLibrary> lib_;
  const size_t block_size_;
  std::mutex mu_;
  std::vector<void*> free_;
  // Blocks allocated and not yet freed, in use or in `free_`.
  size_t owned_ = 0;
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
// H.264 access unit, a VP8 or VP9 frame, an AV1 temporal unit); with no display
// delay the parser decodes it and maps the picture before cuvidParseVideoData
// returns, so a frame comes out of the Decode() call that put it in, as the
// software decoders do. A frame that is not shown (a hidden VP9 or AV1 frame)
// gives no picture. The picture is converted to RGB on the GPU and copied off
// it as an RgbFrameBuffer, which the frame sink and the standalone decoder hand
// out as it is and any other consumer reads as I420; where the kernel did not
// load, it is copied off as NV12 instead. A stream the GPU does not decode
// (high bit depth, 4:2:2 or 4:4:4, a coded size outside its limits) or a
// decoder it cannot create returns WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE, for a
// software decoder to take over.
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
    // The decoder's own stream, so each decoder's copies and conversions run
    // beside other decoders' instead of queueing behind them on the default
    // stream. Without one, they run on the default stream.
    if (lib_->cu->cuStreamCreate(&stream_, CU_STREAM_NON_BLOCKING) !=
        CUDA_SUCCESS) {
      stream_ = nullptr;
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
    if (stream_) {
      lib_->cu->cuStreamDestroy(stream_);
      stream_ = nullptr;
    }
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Decode(const webrtc::EncodedImage& input,
                 int64_t /*render_time_ms*/) override {
    if (!parser_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    if (input.size() == 0) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
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
    const CUresult parsed = lib_->cuvid->cuvidParseVideoData(parser_, &packet);
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
    proc.output_stream = stream_;
    CUdeviceptr frame = 0;
    unsigned int pitch = 0;
    if (lib_->cuvid->cuvidMapVideoFrame(decoder_, display->picture_index, &frame,
                                        &pitch, &proc) != CUDA_SUCCESS) {
      failed_ = true;
      return 0;
    }
    // RGB made on the GPU, or else NV12 for the CPU to convert: the kernel
    // did not load, or this picture's conversion failed.
    webrtc::scoped_refptr<webrtc::VideoFrameBuffer> out;
    if (lib_->nv12_to_rgb) out = ToRgb(frame, pitch);
    if (!out) {
      webrtc::scoped_refptr<webrtc::NV12Buffer> nv12 =
          buffer_pool_.CreateNV12Buffer(width_, height_);
      if (nv12 && Copy(frame, pitch, nv12.get())) out = nv12;
    }
    // A picture made has synced the stream; a failure may leave the kernel
    // queued, reading the surface, so it finishes before the unmap.
    if (!out) lib_->cu->cuStreamSynchronize(stream_);
    lib_->cuvid->cuvidUnmapVideoFrame(decoder_, frame);
    if (!out) {
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
      callback_->Decoded(decoded);
    }
    produced_ = true;
    return 1;
  }

  // The mapped surface converted to packed RGB on the GPU and copied to a
  // page-locked block, as an RgbFrameBuffer; null when a step fails. All of it
  // runs on the decoder's stream, after the map's own work there.
  webrtc::scoped_refptr<webrtc::VideoFrameBuffer> ToRgb(CUdeviceptr frame,
                                                        unsigned int pitch) {
    const size_t stride = static_cast<size_t>(width_) * 3;
    const size_t bytes = stride * height_;
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
    CUdeviceptr uv = frame + static_cast<CUdeviceptr>(pitch) * surface_height_;
    unsigned int plane_pitch = pitch;
    unsigned int out_pitch = static_cast<unsigned int>(stride);
    int width = width_, height = height_;
    void* args[] = {&frame, &plane_pitch, &uv,     &plane_pitch,
                    &rgb_device_, &out_pitch, &width, &height};
    constexpr unsigned int kBlockX = 32, kBlockY = 8;
    if (lib_->cu->cuLaunchKernel(lib_->nv12_to_rgb,
                                 (width + kBlockX - 1) / kBlockX,
                                 (height + kBlockY - 1) / kBlockY, 1, kBlockX,
                                 kBlockY, 1, 0, stream_, args,
                                 nullptr) != CUDA_SUCCESS) {
      return nullptr;
    }
    if (!rgb_pool_ || rgb_pool_->block_size() != bytes)
      rgb_pool_ = std::make_shared<PinnedPool>(lib_, bytes);
    std::shared_ptr<PinnedPool> pool = rgb_pool_;
    void* block = pool->Take();
    std::function<void()> release;
    if (block) {
      release = [pool, block] { pool->Give(block); };
    } else {
      // No page-locked block to spare: an ordinary one, copied to more slowly.
      block = std::malloc(bytes);
      if (!block) return nullptr;
      release = [block] { std::free(block); };
    }
    const bool queued = lib_->cu->cuMemcpyDtoHAsync(block, rgb_device_, bytes,
                                                    stream_) == CUDA_SUCCESS;
    // Whatever was queued has finished before the block can go back to the
    // pool, or the surface be unmapped, whether the copy queued or not.
    const bool synced = lib_->cu->cuStreamSynchronize(stream_) == CUDA_SUCCESS;
    if (!queued || !synced) {
      release();
      return nullptr;
    }
    return webrtc::make_ref_counted<RgbFrameBuffer>(
        width_, height_, static_cast<const uint8_t*>(block),
        static_cast<int>(stride), std::move(release));
  }

  // The mapped surface's luma and interleaved chroma planes into `out`, on
  // the decoder's stream, after the map's own work there.
  bool Copy(CUdeviceptr frame, unsigned int pitch, webrtc::NV12Buffer* out) {
    CUDA_MEMCPY2D copy;
    memset(&copy, 0, sizeof(copy));
    copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.srcDevice = frame;
    copy.srcPitch = pitch;
    copy.dstMemoryType = CU_MEMORYTYPE_HOST;
    copy.dstHost = out->MutableDataY();
    copy.dstPitch = out->StrideY();
    copy.WidthInBytes = width_;
    copy.Height = height_;
    const bool luma = lib_->cu->cuMemcpy2DAsync(&copy, stream_) == CUDA_SUCCESS;
    bool chroma = false;
    if (luma) {
      // Chroma follows the luma of the whole output surface.
      copy.srcDevice =
          frame + static_cast<CUdeviceptr>(pitch) * surface_height_;
      copy.dstHost = out->MutableDataUV();
      copy.dstPitch = out->StrideUV();
      copy.WidthInBytes = 2 * out->ChromaWidth();
      copy.Height = out->ChromaHeight();
      chroma = lib_->cu->cuMemcpy2DAsync(&copy, stream_) == CUDA_SUCCESS;
    }
    // Whatever was queued has finished before `out` can go back to the pool,
    // or the surface be unmapped, whether every copy queued or not.
    const bool synced = lib_->cu->cuStreamSynchronize(stream_) == CUDA_SUCCESS;
    return luma && chroma && synced;
  }

  std::shared_ptr<NvdecLibrary> lib_;
  const cudaVideoCodec codec_;
  const NvdecLibrary::Caps caps_;
  // The decoded pictures' buffers, reused once their consumers drop them.
  webrtc::VideoFrameBufferPool buffer_pool_{/*zero_initialize=*/false,
                                            kMaxBuffers};
  // The stream this decoder's map, conversion and copies run on; null for the
  // default stream.
  CUstream stream_ = nullptr;
  // The kernel's output on the GPU, and the page-locked blocks its pictures
  // are copied to, both sized for the current picture.
  CUdeviceptr rgb_device_ = 0;
  size_t rgb_bytes_ = 0;
  std::shared_ptr<PinnedPool> rgb_pool_;
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
    if (lib->cu->cuModuleLoadData(&lib->rgb_module, kNv12ToRgbPtx) !=
            CUDA_SUCCESS ||
        lib->cu->cuModuleGetFunction(&lib->nv12_to_rgb, lib->rgb_module,
                                     "nv12_to_rgb24") != CUDA_SUCCESS) {
      RTC_LOG(LS_WARNING)
          << "NVDEC: the NV12 to RGB kernel did not load on GPU " << device
          << "; its pictures are converted on the CPU instead";
      if (lib->rgb_module) lib->cu->cuModuleUnload(lib->rgb_module);
      lib->rgb_module = nullptr;
      lib->nv12_to_rgb = nullptr;
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
