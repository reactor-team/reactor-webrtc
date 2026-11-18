#include "nvdec_codec.h"

#include <cstring>
#include <utility>
#include <vector>

#include "api/video/encoded_image.h"
#include "api/video/nv12_buffer.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_decoder.h"
#include "modules/video_coding/include/video_error_codes.h"

// The loader reports a library or symbol it cannot load on stderr by default.
// A machine without an NVIDIA driver is an expected case here, not an error,
// so its messages are dropped; OpenNvdec reports the reason instead.
#define FFNV_LOG_FUNC(logctx, msg, ...) ((void)0)
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...) ((void)0)
#include "vendor/dynlink_loader.h"

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

void SetWhy(std::string* why, std::string reason) {
  if (why) *why = std::move(reason);
}

// The H.264 profiles NVDEC decodes, at level 3.1, the level libwebrtc lists
// its own H.264 at. A decoder accepts any level its profile allows.
std::vector<webrtc::SdpVideoFormat> NvdecFormats() {
  std::vector<webrtc::SdpVideoFormat> formats;
  for (const char* profile_level_id : {"42e01f",   // constrained baseline
                                       "42001f",   // baseline
                                       "4d001f",   // main
                                       "64001f"}) {  // high
    formats.emplace_back("H264",
                         webrtc::CodecParameterMap{{"level-asymmetry-allowed", "1"},
                                                   {"packetization-mode", "1"},
                                                   {"profile-level-id", profile_level_id}});
  }
  return formats;
}

// One H.264 stream. libwebrtc hands Decode() one access unit at a time; with no
// display delay the parser decodes it and maps the picture before
// cuvidParseVideoData returns, so a frame comes out of the Decode() call that
// put its access unit in, as the software decoders do. The picture is copied
// from the GPU as NV12; consumers that want I420 or RGB convert from there.
class NvdecH264Decoder : public webrtc::VideoDecoder {
 public:
  explicit NvdecH264Decoder(std::shared_ptr<NvdecLibrary> lib)
      : lib_(std::move(lib)) {}

  ~NvdecH264Decoder() override { Release(); }

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
    // One access unit: the parser decodes it now rather than waiting for the
    // start of the next one to know it ended.
    packet.flags = CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE;
    packet.timestamp = input.RtpTimestamp();

    failed_ = false;
    produced_ = false;
    const CUresult parsed = lib_->cuvid->cuvidParseVideoData(parser_, &packet);
    if (parsed != CUDA_SUCCESS || failed_) {
      // Start over at the next key frame: a reference lost here would corrupt
      // every frame until then. libwebrtc asks the sender for one on error.
      DestroyParserAndDecoder();
      CreateParser();
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    return produced_ ? WEBRTC_VIDEO_CODEC_OK : WEBRTC_VIDEO_CODEC_NO_OUTPUT;
  }

  DecoderInfo GetDecoderInfo() const override {
    return {"NVDEC", /*is_hardware_accelerated=*/true};
  }

 private:
  static int CUDAAPI OnSequence(void* self, CUVIDEOFORMAT* format) {
    return static_cast<NvdecH264Decoder*>(self)->HandleSequence(format);
  }
  static int CUDAAPI OnDecode(void* self, CUVIDPICPARAMS* picture) {
    return static_cast<NvdecH264Decoder*>(self)->HandleDecode(picture);
  }
  static int CUDAAPI OnDisplay(void* self, CUVIDPARSERDISPINFO* display) {
    return static_cast<NvdecH264Decoder*>(self)->HandleDisplay(display);
  }

  bool CreateParser() {
    CUVIDPARSERPARAMS params;
    memset(&params, 0, sizeof(params));
    params.CodecType = cudaVideoCodec_H264;
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

  // A new sequence, or a change of format: (re)creates the decoder for it.
  // Returns the number of decode surfaces the parser should cycle through,
  // 0 to fail.
  int HandleSequence(CUVIDEOFORMAT* format) {
    if (format->codec != cudaVideoCodec_H264 ||
        format->chroma_format != cudaVideoChromaFormat_420 ||
        format->bit_depth_luma_minus8 != 0) {
      failed_ = true;  // 4:2:2, 4:4:4 or high bit depth: not 8-bit NV12.
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
    info.CodecType = cudaVideoCodec_H264;
    info.ChromaFormat = cudaVideoChromaFormat_420;
    info.bitDepthMinus8 = 0;
    info.ulCreationFlags = cudaVideoCreate_PreferCUVID;
    info.OutputFormat = cudaVideoSurfaceFormat_NV12;
    info.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
    info.display_area.left = static_cast<short>(format->display_area.left);
    info.display_area.top = static_cast<short>(format->display_area.top);
    info.display_area.right = static_cast<short>(format->display_area.right);
    info.display_area.bottom = static_cast<short>(format->display_area.bottom);
    // The output is the display area, cropped by the decoder; NV12 needs even
    // dimensions, and the odd last column or row is dropped when they are not.
    info.ulTargetWidth = (width + 1) & ~1;
    info.ulTargetHeight = (height + 1) & ~1;
    info.ulNumOutputSurfaces = 1;
    info.vidLock = lock_;
    if (lib_->cuvid->cuvidCreateDecoder(&decoder_, &info) != CUDA_SUCCESS) {
      decoder_ = nullptr;
      failed_ = true;
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
    CUdeviceptr frame = 0;
    unsigned int pitch = 0;
    if (lib_->cuvid->cuvidMapVideoFrame(decoder_, display->picture_index, &frame,
                                        &pitch, &proc) != CUDA_SUCCESS) {
      failed_ = true;
      return 0;
    }
    webrtc::scoped_refptr<webrtc::NV12Buffer> out =
        webrtc::NV12Buffer::Create(width_, height_);
    const bool copied = Copy(frame, pitch, out.get());
    lib_->cuvid->cuvidUnmapVideoFrame(decoder_, frame);
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
      callback_->Decoded(decoded);
    }
    produced_ = true;
    return 1;
  }

  // The mapped surface's luma and interleaved chroma planes into `out`.
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
    if (lib_->cu->cuMemcpy2D(&copy) != CUDA_SUCCESS) return false;

    // Chroma follows the luma of the whole output surface.
    copy.srcDevice = frame + static_cast<CUdeviceptr>(pitch) * surface_height_;
    copy.dstHost = out->MutableDataUV();
    copy.dstPitch = out->StrideUV();
    copy.WidthInBytes = 2 * out->ChromaWidth();
    copy.Height = out->ChromaHeight();
    return lib_->cu->cuMemcpy2D(&copy) == CUDA_SUCCESS;
  }

  std::shared_ptr<NvdecLibrary> lib_;
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
  // Set by the callbacks during one cuvidParseVideoData call.
  bool failed_ = false;
  bool produced_ = false;
};

class NvdecVideoDecoderFactory : public webrtc::VideoDecoderFactory {
 public:
  explicit NvdecVideoDecoderFactory(std::shared_ptr<NvdecLibrary> lib)
      : lib_(std::move(lib)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return NvdecFormats();
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& /*env*/,
      const webrtc::SdpVideoFormat& format) override {
    if (format.name != "H264") return nullptr;
    return std::make_unique<NvdecH264Decoder>(lib_);
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

  CUVIDDECODECAPS caps;
  memset(&caps, 0, sizeof(caps));
  caps.eCodecType = cudaVideoCodec_H264;
  caps.eChromaFormat = cudaVideoChromaFormat_420;
  caps.nBitDepthMinus8 = 0;
  {
    ScopedContext scope(*lib);
    if (!scope.ok() ||
        lib->cuvid->cuvidGetDecoderCaps(&caps) != CUDA_SUCCESS ||
        !caps.bIsSupported) {
      SetWhy(why, "GPU " + std::to_string(device) + " does not decode H.264");
      return nullptr;
    }
  }
  return lib;
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateNvdecVideoDecoderFactory(
    std::shared_ptr<NvdecLibrary> lib) {
  return std::make_unique<NvdecVideoDecoderFactory>(std::move(lib));
}

}  // namespace reactor
