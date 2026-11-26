#include "nvenc_codec.h"

#include <algorithm>
#include <cmath>
#include <cstring>
#include <memory>
#include <optional>
#include <utility>
#include <vector>

#include "api/transport/rtp/dependency_descriptor.h"
#include "api/video/encoded_image.h"
#include "api/video/i420_buffer.h"
#include "api/video/nv12_buffer.h"
#include "api/video/render_resolution.h"
#include "api/video/video_frame.h"
#include "api/video/video_frame_buffer.h"
#include "api/video_codecs/scalability_mode.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_codec.h"
#include "api/video_codecs/video_encoder.h"
#include "common_video/generic_frame_descriptor/generic_frame_info.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"
#include "third_party/libyuv/include/libyuv/planar_functions.h"

// The loader reports a library or symbol it cannot load on stderr by default.
// A machine without an NVIDIA driver or encoder is an expected case here, not
// an error, so its messages are dropped; OpenNvenc reports the reason instead.
#define FFNV_LOG_FUNC(logctx, msg, ...) ((void)0)
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...) ((void)0)
#include "../nvdec/vendor/dynlink_loader.h"

namespace reactor {

class NvencLibrary {
 public:
  ~NvencLibrary() {
    if (cu && context) cu->cuDevicePrimaryCtxRelease(device);
    nvenc_free_functions(&nvenc);
    cuda_free_functions(&cu);
  }

  CudaFunctions* cu = nullptr;
  NvencFunctions* nvenc = nullptr;
  NV_ENCODE_API_FUNCTION_LIST api;
  CUdevice device = 0;
  CUcontext context = nullptr;
  // Whether the GPU encodes a codec at 8-bit 4:2:0, the sizes it encodes it
  // at, and whether it takes planar I420 (IYUV) input, asked once at open.
  // Every NVENC takes NV12.
  struct Caps {
    bool encodes = false;
    int min_width = 0;
    int min_height = 0;
    int max_width = 0;
    int max_height = 0;
    bool takes_iyuv = false;
  };
  Caps h264;
  Caps av1;
};

namespace {

// The preset and tuning every encoder runs: the NVENC presets run from P1
// (fastest) to P7 (best compressing); with ultra-low-latency tuning P1 to P4
// are NVIDIA's choice for real-time streaming. Ultra-low-latency tuning has
// no B-frames, no look-ahead and no reordering.
const GUID& kPreset = NV_ENC_PRESET_P4_GUID;
constexpr NV_ENC_TUNING_INFO kTuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

bool SameGuid(const GUID& a, const GUID& b) {
  return std::memcmp(&a, &b, sizeof(GUID)) == 0;
}

void SetWhy(std::string* why, std::string reason) {
  if (why) *why = std::move(reason);
}

// Makes the library's context current on this thread for the scope. The
// uploads need it, and libwebrtc encodes on a thread of its own.
class ScopedContext {
 public:
  explicit ScopedContext(const NvencLibrary& lib) : lib_(lib) {
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
  const NvencLibrary& lib_;
  bool pushed_ = false;
};

// An encode session on `lib`'s context, or null. Its context must be current.
void* OpenSession(const NvencLibrary& lib) {
  NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS params;
  std::memset(&params, 0, sizeof(params));
  params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
  params.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
  params.device = lib.context;
  params.apiVersion = NVENCAPI_VERSION;
  void* session = nullptr;
  if (lib.api.nvEncOpenEncodeSessionEx(&params, &session) != NV_ENC_SUCCESS) {
    // A failed open still hands back a session to destroy.
    if (session) lib.api.nvEncDestroyEncoder(session);
    return nullptr;
  }
  return session;
}

int AskCap(const NvencLibrary& lib, void* session, const GUID& codec,
           NV_ENC_CAPS cap, int fallback) {
  NV_ENC_CAPS_PARAM param;
  std::memset(&param, 0, sizeof(param));
  param.version = NV_ENC_CAPS_PARAM_VER;
  param.capsToQuery = cap;
  int value = 0;
  if (lib.api.nvEncGetEncodeCaps(session, codec, &param, &value) !=
      NV_ENC_SUCCESS) {
    return fallback;
  }
  return value;
}

// Whether `session`'s GPU encodes `codec`, at which sizes, and from which
// input formats.
NvencLibrary::Caps AskEncodes(const NvencLibrary& lib, void* session,
                              const GUID& codec) {
  NvencLibrary::Caps out;
  uint32_t count = 0;
  if (lib.api.nvEncGetEncodeGUIDCount(session, &count) != NV_ENC_SUCCESS ||
      count == 0) {
    return out;
  }
  std::vector<GUID> codecs(count);
  if (lib.api.nvEncGetEncodeGUIDs(session, codecs.data(), count, &count) !=
      NV_ENC_SUCCESS) {
    return out;
  }
  codecs.resize(count);
  if (std::none_of(codecs.begin(), codecs.end(),
                   [&](const GUID& g) { return SameGuid(g, codec); })) {
    return out;
  }
  uint32_t format_count = 0;
  if (lib.api.nvEncGetInputFormatCount(session, codec, &format_count) !=
      NV_ENC_SUCCESS) {
    return out;
  }
  std::vector<NV_ENC_BUFFER_FORMAT> formats(format_count);
  if (format_count > 0 &&
      lib.api.nvEncGetInputFormats(session, codec, formats.data(),
                                   format_count,
                                   &format_count) != NV_ENC_SUCCESS) {
    return out;
  }
  formats.resize(format_count);
  auto takes = [&](NV_ENC_BUFFER_FORMAT f) {
    return std::find(formats.begin(), formats.end(), f) != formats.end();
  };
  if (!takes(NV_ENC_BUFFER_FORMAT_NV12)) return out;
  out.takes_iyuv = takes(NV_ENC_BUFFER_FORMAT_IYUV);
  out.max_width = AskCap(lib, session, codec, NV_ENC_CAPS_WIDTH_MAX, 0);
  out.max_height = AskCap(lib, session, codec, NV_ENC_CAPS_HEIGHT_MAX, 0);
  // Drivers before the minimum caps existed answer an error: no minimum.
  out.min_width = AskCap(lib, session, codec, NV_ENC_CAPS_WIDTH_MIN, 1);
  out.min_height = AskCap(lib, session, codec, NV_ENC_CAPS_HEIGHT_MIN, 1);
  out.encodes = out.max_width > 0 && out.max_height > 0;
  return out;
}

// The H.264 format NVENC encodes: constrained baseline (no B-frames, CAVLC)
// in packetization mode 1, as OpenH264 lists it, so a peer sees the same
// profile whichever encoder this process runs.
webrtc::SdpVideoFormat H264Format() {
  return webrtc::SdpVideoFormat("H264", {{"level-asymmetry-allowed", "1"},
                                         {"packetization-mode", "1"},
                                         {"profile-level-id", "42e01f"}});
}

std::vector<webrtc::SdpVideoFormat> NvencFormats(const NvencLibrary& lib) {
  std::vector<webrtc::SdpVideoFormat> formats;
  if (lib.h264.encodes) formats.push_back(H264Format());
  if (lib.av1.encodes) formats.push_back(webrtc::SdpVideoFormat::AV1Profile0());
  return formats;
}

// AV1 is sent with the dependency descriptor, which libwebrtc builds from the
// frame's generic info rather than from codec specifics: one spatial and one
// temporal layer, each frame referencing the one before, as libaom's encoder
// describes its L1T1 stream (ScalableVideoControllerNoLayering).
webrtc::FrameDependencyStructure Av1L1T1Structure(int width, int height) {
  webrtc::FrameDependencyStructure structure;
  structure.num_decode_targets = 1;
  structure.num_chains = 1;
  structure.decode_target_protected_by_chain = {0};
  webrtc::FrameDependencyTemplate key_frame;
  key_frame.decode_target_indications = {
      webrtc::DecodeTargetIndication::kSwitch};
  key_frame.chain_diffs = {0};
  structure.templates.push_back(key_frame);
  webrtc::FrameDependencyTemplate delta_frame;
  delta_frame.decode_target_indications = {
      webrtc::DecodeTargetIndication::kSwitch};
  delta_frame.chain_diffs = {1};
  delta_frame.frame_diffs = {1};
  structure.templates.push_back(delta_frame);
  structure.resolutions = {webrtc::RenderResolution(width, height)};
  return structure;
}

// One stream of one codec on one NVENC session. Each Encode() uploads the
// frame to the GPU and encodes it synchronously: the encoded frame is handed
// to the callback before Encode() returns, as the software encoders do. The
// stream has no B-frames and no periodic key frame (an infinite GOP), with a
// key frame only when one is asked for, and carries its sequence header
// (H.264 SPS/PPS, the AV1 sequence header OBU) on every key frame. Rate
// control is CBR at the rate SetRates() last gave, so the encoded size of each
// frame stays near the target. InitEncode fails for a size the GPU does not
// encode, an odd size (4:2:0 needs even dimensions) or a session the GPU
// cannot open, for a software encoder to take over; a session that fails
// mid-stream answers WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE.
class NvencVideoEncoder : public webrtc::VideoEncoder {
 public:
  NvencVideoEncoder(std::shared_ptr<NvencLibrary> lib,
                    webrtc::VideoCodecType codec)
      : lib_(std::move(lib)),
        codec_(codec),
        caps_(codec == webrtc::kVideoCodecAV1 ? lib_->av1 : lib_->h264) {}

  ~NvencVideoEncoder() override { Release(); }

  int InitEncode(const webrtc::VideoCodec* settings,
                 const Settings& /*encoder_settings*/) override {
    Release();
    if (!settings || settings->codecType != codec_ ||
        settings->numberOfSimulcastStreams > 1) {
      return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    }
    if (settings->maxBitrate > 0) {
      bitrate_bps_ = settings->maxBitrate * 1000;
    } else if (settings->startBitrate > 0) {
      bitrate_bps_ = settings->startBitrate * 1000;
    }
    if (settings->maxFramerate > 0) framerate_ = settings->maxFramerate;
    return Open(settings->width, settings->height);
  }

  int32_t RegisterEncodeCompleteCallback(
      webrtc::EncodedImageCallback* callback) override {
    callback_ = callback;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Release() override {
    if (!session_) return WEBRTC_VIDEO_CODEC_OK;
    ScopedContext scope(*lib_);
    if (bitstream_) lib_->api.nvEncDestroyBitstreamBuffer(session_, bitstream_);
    if (registered_) lib_->api.nvEncUnregisterResource(session_, registered_);
    lib_->api.nvEncDestroyEncoder(session_);
    if (input_) lib_->cu->cuMemFree(input_);
    session_ = nullptr;
    bitstream_ = nullptr;
    registered_ = nullptr;
    input_ = 0;
    input_pitch_ = 0;
    width_ = height_ = 0;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Encode(
      const webrtc::VideoFrame& frame,
      const std::vector<webrtc::VideoFrameType>* frame_types) override {
    if (!session_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    if (!callback_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    webrtc::scoped_refptr<webrtc::VideoFrameBuffer> buffer =
        frame.video_frame_buffer();
    if (buffer->width() != width_ || buffer->height() != height_) {
      // A new size is a new sequence, starting at a key frame.
      const int rc = Open(buffer->width(), buffer->height());
      if (rc != WEBRTC_VIDEO_CODEC_OK) return rc;
    }
    bool key_frame = force_key_frame_;
    if (frame_types) {
      for (webrtc::VideoFrameType type : *frame_types) {
        if (type == webrtc::VideoFrameType::kVideoFrameKey) key_frame = true;
      }
    }

    ScopedContext scope(*lib_);
    if (!scope.ok() || !Upload(*buffer)) {
      RTC_LOG(LS_WARNING) << "NVENC: the frame did not reach the GPU";
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }

    NV_ENC_MAP_INPUT_RESOURCE map;
    std::memset(&map, 0, sizeof(map));
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = registered_;
    if (lib_->api.nvEncMapInputResource(session_, &map) != NV_ENC_SUCCESS) {
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }

    NV_ENC_PIC_PARAMS picture;
    std::memset(&picture, 0, sizeof(picture));
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = static_cast<uint32_t>(width_);
    picture.inputHeight = static_cast<uint32_t>(height_);
    picture.inputPitch = static_cast<uint32_t>(input_pitch_);
    picture.inputBuffer = map.mappedResource;
    picture.outputBitstream = bitstream_;
    picture.bufferFmt = input_format_;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.inputTimeStamp = frame.rtp_timestamp();
    if (key_frame) {
      picture.encodePicFlags =
          NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    }
    const NVENCSTATUS encoded =
        lib_->api.nvEncEncodePicture(session_, &picture);
    if (encoded != NV_ENC_SUCCESS) {
      lib_->api.nvEncUnmapInputResource(session_, map.mappedResource);
      if (encoded == NV_ENC_ERR_NEED_MORE_INPUT) {
        // Not with this configuration (no B-frames, no look-ahead), but if a
        // driver holds the frame back, nothing comes out of it.
        callback_->OnFrameDropped(frame.rtp_timestamp(), 0, true);
        return WEBRTC_VIDEO_CODEC_NO_OUTPUT;
      }
      RTC_LOG(LS_WARNING) << "NVENC: nvEncEncodePicture failed: " << encoded;
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }

    NV_ENC_LOCK_BITSTREAM lock;
    std::memset(&lock, 0, sizeof(lock));
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = bitstream_;
    if (lib_->api.nvEncLockBitstream(session_, &lock) != NV_ENC_SUCCESS) {
      lib_->api.nvEncUnmapInputResource(session_, map.mappedResource);
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
    webrtc::scoped_refptr<webrtc::EncodedImageBuffer> data =
        webrtc::EncodedImageBuffer::Create(
            static_cast<const uint8_t*>(lock.bitstreamBufferPtr),
            lock.bitstreamSizeInBytes);
    const bool is_key =
        key_frame || lock.pictureType == NV_ENC_PIC_TYPE_IDR;
    const uint32_t average_qp = lock.frameAvgQP;
    lib_->api.nvEncUnlockBitstream(session_, bitstream_);
    lib_->api.nvEncUnmapInputResource(session_, map.mappedResource);
    force_key_frame_ = false;
    if (data->size() == 0) {
      callback_->OnFrameDropped(frame.rtp_timestamp(), 0, true);
      return WEBRTC_VIDEO_CODEC_OK;
    }

    webrtc::EncodedImage image;
    image.SetEncodedData(data);
    image.SetRtpTimestamp(frame.rtp_timestamp());
    image.capture_time_ms_ = frame.render_time_ms();
    image.SetFrameType(is_key ? webrtc::VideoFrameType::kVideoFrameKey
                              : webrtc::VideoFrameType::kVideoFrameDelta);
    image._encodedWidth = static_cast<uint32_t>(width_);
    image._encodedHeight = static_cast<uint32_t>(height_);
    image.rotation_ = frame.rotation();
    image.content_type_ = webrtc::VideoContentType::UNSPECIFIED;
    image.timing_.flags = webrtc::VideoSendTiming::kInvalid;
    image.SetColorSpace(frame.color_space());

    webrtc::CodecSpecificInfo info;
    info.codecType = codec_;
    if (codec_ == webrtc::kVideoCodecH264) {
      // H.264 QP, 0 to 51, as libwebrtc's own H.264 encoders report it.
      image.qp_ = static_cast<int>(average_qp);
      info.codecSpecific.H264.packetization_mode =
          webrtc::H264PacketizationMode::NonInterleaved;
      info.codecSpecific.H264.temporal_idx = webrtc::kNoTemporalIdx;
      info.codecSpecific.H264.idr_frame = is_key;
      info.codecSpecific.H264.base_layer_sync = false;
    } else {
      info.scalability_mode = webrtc::ScalabilityMode::kL1T1;
      webrtc::GenericFrameInfo generic;
      generic.decode_target_indications = {
          webrtc::DecodeTargetIndication::kSwitch};
      generic.part_of_chain = {true};
      generic.encoder_buffers = {webrtc::CodecBufferUsage(
          /*id=*/0, /*referenced=*/!is_key, /*updated=*/true)};
      info.generic_frame_info = generic;
      if (is_key) info.template_structure = Av1L1T1Structure(width_, height_);
    }
    info.end_of_picture = true;
    callback_->OnEncodedImage(image, &info);
    return WEBRTC_VIDEO_CODEC_OK;
  }

  void SetRates(const RateControlParameters& params) override {
    const uint32_t bitrate_bps = params.bitrate.get_sum_bps();
    // A zero rate pauses the stream: libwebrtc stops sending frames, and the
    // encoder keeps its last rate for when they resume.
    if (bitrate_bps == 0) return;
    bitrate_bps_ = bitrate_bps;
    if (params.framerate_fps >= 1.0) framerate_ = params.framerate_fps;
    if (!session_) return;
    ApplyRates();
    NV_ENC_RECONFIGURE_PARAMS reconfigure;
    std::memset(&reconfigure, 0, sizeof(reconfigure));
    reconfigure.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    reconfigure.reInitEncodeParams = init_;
    ScopedContext scope(*lib_);
    if (lib_->api.nvEncReconfigureEncoder(session_, &reconfigure) !=
        NV_ENC_SUCCESS) {
      RTC_LOG(LS_WARNING) << "NVENC: the new rate was not applied ("
                          << bitrate_bps_ << " bps, " << framerate_ << " fps)";
    }
  }

  EncoderInfo GetEncoderInfo() const override {
    EncoderInfo info;
    info.implementation_name = "NVENC";
    info.is_hardware_accelerated = true;
    info.supports_native_handle = false;
    info.supports_simulcast = false;
    // 4:2:0 needs even dimensions; libwebrtc scales to them.
    info.requested_resolution_alignment = 2;
    info.preferred_pixel_formats = {webrtc::VideoFrameBuffer::Type::kI420};
    return info;
  }

 private:
  // (Re)opens the session at `width`x`height` with the current rates. Its
  // first frame is a key frame.
  int Open(int width, int height) {
    Release();
    if (width <= 0 || height <= 0 || width % 2 != 0 || height % 2 != 0 ||
        width < caps_.min_width || height < caps_.min_height ||
        width > caps_.max_width || height > caps_.max_height) {
      // Odd or outside what the GPU encodes: software does.
      return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    }
    ScopedContext scope(*lib_);
    if (!scope.ok()) return WEBRTC_VIDEO_CODEC_ERROR;
    session_ = OpenSession(*lib_);
    if (!session_) {
      // Out of encode sessions (a GeForce allows a few) or GPU memory.
      RTC_LOG(LS_WARNING) << "NVENC: no encode session on the GPU";
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    width_ = width;
    height_ = height;
    if (!Configure() || !AllocateInput()) {
      Release();
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    NV_ENC_CREATE_BITSTREAM_BUFFER bitstream;
    std::memset(&bitstream, 0, sizeof(bitstream));
    bitstream.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    if (lib_->api.nvEncCreateBitstreamBuffer(session_, &bitstream) !=
        NV_ENC_SUCCESS) {
      Release();
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    bitstream_ = bitstream.bitstreamBuffer;
    force_key_frame_ = true;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  const GUID& CodecGuid() const {
    return codec_ == webrtc::kVideoCodecAV1 ? NV_ENC_CODEC_AV1_GUID
                                            : NV_ENC_CODEC_H264_GUID;
  }

  // The preset's configuration with this stream's choices on top, then the
  // encoder initialised with it.
  bool Configure() {
    NV_ENC_PRESET_CONFIG preset;
    std::memset(&preset, 0, sizeof(preset));
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (lib_->api.nvEncGetEncodePresetConfigEx(session_, CodecGuid(), kPreset,
                                               kTuning,
                                               &preset) != NV_ENC_SUCCESS) {
      return false;
    }
    config_ = preset.presetCfg;
    config_.version = NV_ENC_CONFIG_VER;
    config_.gopLength = NVENC_INFINITE_GOPLENGTH;
    config_.frameIntervalP = 1;  // No B-frames.
    config_.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config_.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
    config_.rcParams.enableLookahead = 0;
    config_.rcParams.zeroReorderDelay = 1;
    if (codec_ == webrtc::kVideoCodecAV1) {
      config_.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
      NV_ENC_CONFIG_AV1& av1 = config_.encodeCodecConfig.av1Config;
      av1.idrPeriod = NVENC_INFINITE_GOPLENGTH;
      av1.repeatSeqHdr = 1;
      av1.chromaFormatIDC = 1;
      av1.outputAnnexBFormat = 0;  // Low-overhead OBUs, what RTP carries.
      av1.inputBitDepth = NV_ENC_BIT_DEPTH_8;
      av1.outputBitDepth = NV_ENC_BIT_DEPTH_8;
    } else {
      config_.profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;
      NV_ENC_CONFIG_H264& h264 = config_.encodeCodecConfig.h264Config;
      h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
      h264.repeatSPSPPS = 1;
      h264.chromaFormatIDC = 1;
      h264.level = NV_ENC_LEVEL_AUTOSELECT;
      h264.outputAUD = 0;
      h264.sliceMode = 0;
      h264.sliceModeData = 0;  // One slice per picture.
      h264.inputBitDepth = NV_ENC_BIT_DEPTH_8;
      h264.outputBitDepth = NV_ENC_BIT_DEPTH_8;
    }

    std::memset(&init_, 0, sizeof(init_));
    init_.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init_.encodeGUID = CodecGuid();
    init_.presetGUID = kPreset;
    init_.tuningInfo = kTuning;
    init_.encodeWidth = static_cast<uint32_t>(width_);
    init_.encodeHeight = static_cast<uint32_t>(height_);
    init_.darWidth = static_cast<uint32_t>(width_);
    init_.darHeight = static_cast<uint32_t>(height_);
    init_.maxEncodeWidth = static_cast<uint32_t>(width_);
    init_.maxEncodeHeight = static_cast<uint32_t>(height_);
    init_.enablePTD = 1;
    init_.enableEncodeAsync = 0;
    init_.encodeConfig = &config_;
    ApplyRates();
    const NVENCSTATUS status =
        lib_->api.nvEncInitializeEncoder(session_, &init_);
    if (status != NV_ENC_SUCCESS) {
      RTC_LOG(LS_WARNING) << "NVENC: nvEncInitializeEncoder failed: "
                          << status;
      return false;
    }
    return true;
  }

  // The current bitrate and frame rate into the configuration. The rate
  // buffer holds one frame at the target rate, NVIDIA's setting for
  // low-latency streaming: each frame, key frames included, stays near its
  // share of the rate instead of bursting.
  void ApplyRates() {
    const double fps = framerate_ >= 1.0 ? framerate_ : 30.0;
    init_.frameRateNum = static_cast<uint32_t>(std::lround(fps * 1000));
    init_.frameRateDen = 1000;
    const uint32_t bps = std::max<uint32_t>(bitrate_bps_, 1000);
    config_.rcParams.averageBitRate = bps;
    config_.rcParams.maxBitRate = bps;
    config_.rcParams.vbvBufferSize =
        std::max<uint32_t>(1, static_cast<uint32_t>(bps / fps));
    config_.rcParams.vbvInitialDelay = config_.rcParams.vbvBufferSize;
  }

  // The GPU buffer frames are uploaded to, registered with the session: I420
  // as it is where the GPU takes it, NV12 otherwise.
  bool AllocateInput() {
    input_format_ = caps_.takes_iyuv ? NV_ENC_BUFFER_FORMAT_IYUV
                                     : NV_ENC_BUFFER_FORMAT_NV12;
    size_t pitch = 0;
    // Luma rows, then half as many of chroma: NV12's interleaved plane, or
    // IYUV's two planes at half the pitch.
    if (lib_->cu->cuMemAllocPitch(&input_, &pitch, width_,
                                  height_ + height_ / 2,
                                  /*ElementSizeBytes=*/16) != CUDA_SUCCESS) {
      input_ = 0;
      return false;
    }
    input_pitch_ = static_cast<int>(pitch);
    NV_ENC_REGISTER_RESOURCE resource;
    std::memset(&resource, 0, sizeof(resource));
    resource.version = NV_ENC_REGISTER_RESOURCE_VER;
    resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    resource.width = static_cast<uint32_t>(width_);
    resource.height = static_cast<uint32_t>(height_);
    resource.pitch = static_cast<uint32_t>(input_pitch_);
    resource.resourceToRegister = reinterpret_cast<void*>(input_);
    resource.bufferFormat = input_format_;
    resource.bufferUsage = NV_ENC_INPUT_IMAGE;
    if (lib_->api.nvEncRegisterResource(session_, &resource) !=
        NV_ENC_SUCCESS) {
      return false;
    }
    registered_ = resource.registeredResource;
    return true;
  }

  // One plane from host memory into the input buffer. The copy is synchronous,
  // so the encoder reads a whole frame.
  bool CopyPlane(const uint8_t* src, int src_stride, size_t offset,
                 int dst_pitch, int width_bytes, int rows) {
    CUDA_MEMCPY2D copy;
    std::memset(&copy, 0, sizeof(copy));
    copy.srcMemoryType = CU_MEMORYTYPE_HOST;
    copy.srcHost = src;
    copy.srcPitch = static_cast<size_t>(src_stride);
    copy.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.dstDevice = input_ + offset;
    copy.dstPitch = static_cast<size_t>(dst_pitch);
    copy.WidthInBytes = static_cast<size_t>(width_bytes);
    copy.Height = static_cast<size_t>(rows);
    return lib_->cu->cuMemcpy2D(&copy) == CUDA_SUCCESS;
  }

  // `buffer` into the input buffer, in its format.
  bool Upload(webrtc::VideoFrameBuffer& buffer) {
    const size_t luma = static_cast<size_t>(input_pitch_) * height_;
    const int chroma_width = (width_ + 1) / 2;
    const int chroma_height = (height_ + 1) / 2;
    if (input_format_ == NV_ENC_BUFFER_FORMAT_NV12 &&
        buffer.type() == webrtc::VideoFrameBuffer::Type::kNV12) {
      const webrtc::NV12BufferInterface* nv12 = buffer.GetNV12();
      return CopyPlane(nv12->DataY(), nv12->StrideY(), 0, input_pitch_, width_,
                       height_) &&
             CopyPlane(nv12->DataUV(), nv12->StrideUV(), luma, input_pitch_,
                       2 * chroma_width, chroma_height);
    }
    webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 = buffer.ToI420();
    if (!i420) return false;
    if (!CopyPlane(i420->DataY(), i420->StrideY(), 0, input_pitch_, width_,
                   height_)) {
      return false;
    }
    if (input_format_ == NV_ENC_BUFFER_FORMAT_IYUV) {
      // U then V, each at half the luma pitch.
      const int chroma_pitch = input_pitch_ / 2;
      const size_t v_offset =
          luma + static_cast<size_t>(chroma_pitch) * chroma_height;
      return CopyPlane(i420->DataU(), i420->StrideU(), luma, chroma_pitch,
                       chroma_width, chroma_height) &&
             CopyPlane(i420->DataV(), i420->StrideV(), v_offset, chroma_pitch,
                       chroma_width, chroma_height);
    }
    // NV12 from I420: the chroma planes interleaved on the CPU first.
    uv_.resize(static_cast<size_t>(2 * chroma_width) * chroma_height);
    libyuv::MergeUVPlane(i420->DataU(), i420->StrideU(), i420->DataV(),
                         i420->StrideV(), uv_.data(), 2 * chroma_width,
                         chroma_width, chroma_height);
    return CopyPlane(uv_.data(), 2 * chroma_width, luma, input_pitch_,
                     2 * chroma_width, chroma_height);
  }

  std::shared_ptr<NvencLibrary> lib_;
  const webrtc::VideoCodecType codec_;
  const NvencLibrary::Caps caps_;
  webrtc::EncodedImageCallback* callback_ = nullptr;
  uint32_t bitrate_bps_ = 1000000;
  double framerate_ = 30.0;

  void* session_ = nullptr;
  NV_ENC_CONFIG config_;
  NV_ENC_INITIALIZE_PARAMS init_;
  NV_ENC_OUTPUT_PTR bitstream_ = nullptr;
  NV_ENC_REGISTERED_PTR registered_ = nullptr;
  NV_ENC_BUFFER_FORMAT input_format_ = NV_ENC_BUFFER_FORMAT_NV12;
  CUdeviceptr input_ = 0;
  int input_pitch_ = 0;
  int width_ = 0;
  int height_ = 0;
  bool force_key_frame_ = true;
  // Interleaved chroma for an NV12 upload from I420.
  std::vector<uint8_t> uv_;
};

class NvencVideoEncoderFactory : public webrtc::VideoEncoderFactory {
 public:
  explicit NvencVideoEncoderFactory(std::shared_ptr<NvencLibrary> lib)
      : lib_(std::move(lib)) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return NvencFormats(*lib_);
  }

  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& /*env*/,
      const webrtc::SdpVideoFormat& format) override {
    // Only a format listed: H.264 constrained baseline at any level, AV1
    // main.
    bool listed = false;
    for (const auto& f : NvencFormats(*lib_))
      if (f.IsSameCodec(format)) listed = true;
    if (!listed) return nullptr;
    return std::make_unique<NvencVideoEncoder>(
        lib_, webrtc::PayloadStringToCodecType(format.name));
  }

 private:
  std::shared_ptr<NvencLibrary> lib_;
};

}  // namespace

std::shared_ptr<NvencLibrary> OpenNvenc(int device, std::string* why) {
  auto lib = std::make_shared<NvencLibrary>();
  if (cuda_load_functions(&lib->cu, nullptr) < 0) {
    SetWhy(why, "the NVIDIA driver's libcuda.so.1 did not load");
    return nullptr;
  }
  if (nvenc_load_functions(&lib->nvenc, nullptr) < 0) {
    // In a container, the driver mounts it only with the `video` capability
    // (NVIDIA_DRIVER_CAPABILITIES).
    SetWhy(why, "the NVIDIA driver's libnvidia-encode.so.1 did not load");
    return nullptr;
  }
  // The driver answers (major << 4) | minor: it opens no session for an API
  // newer than that.
  uint32_t max_version = 0;
  if (lib->nvenc->NvEncodeAPIGetMaxSupportedVersion(&max_version) !=
          NV_ENC_SUCCESS ||
      max_version < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
    SetWhy(why, "the NVIDIA driver's NVENC is older than API " +
                    std::to_string(NVENCAPI_MAJOR_VERSION) + "." +
                    std::to_string(NVENCAPI_MINOR_VERSION));
    return nullptr;
  }
  std::memset(&lib->api, 0, sizeof(lib->api));
  lib->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
  if (lib->nvenc->NvEncodeAPICreateInstance(&lib->api) != NV_ENC_SUCCESS ||
      !lib->api.nvEncOpenEncodeSessionEx) {
    SetWhy(why, "NVENC's entry points did not load");
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
    // A session only to ask what the GPU encodes. A GPU without NVENC (an
    // A100, H100 or B200) opens none.
    void* session = OpenSession(*lib);
    if (!session) {
      SetWhy(why, "GPU " + std::to_string(device) +
                      " opened no encode session (it may have no NVENC)");
      return nullptr;
    }
    lib->h264 = AskEncodes(*lib, session, NV_ENC_CODEC_H264_GUID);
    lib->av1 = AskEncodes(*lib, session, NV_ENC_CODEC_AV1_GUID);
    lib->api.nvEncDestroyEncoder(session);
  }
  if (!lib->h264.encodes && !lib->av1.encodes) {
    SetWhy(why, "GPU " + std::to_string(device) + " encodes neither H.264 nor AV1");
    return nullptr;
  }
  return lib;
}

bool NvencEncodes(const NvencLibrary& lib, webrtc::VideoCodecType codec) {
  switch (codec) {
    case webrtc::kVideoCodecH264:
      return lib.h264.encodes;
    case webrtc::kVideoCodecAV1:
      return lib.av1.encodes;
    default:
      return false;
  }
}

std::unique_ptr<webrtc::VideoEncoderFactory> CreateNvencVideoEncoderFactory(
    std::shared_ptr<NvencLibrary> lib) {
  return std::make_unique<NvencVideoEncoderFactory>(std::move(lib));
}

}  // namespace reactor
