#include "openh264_codec.h"

#include <cstring>
#include <utility>
#include <vector>

#if defined(_WIN32)
#include <windows.h>
#else
#include <dlfcn.h>
#endif

#include "api/video/encoded_image.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_decoder.h"
#include "api/video_codecs/video_encoder.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"

// Real ABI declarations (ISVCEncoder/ISVCDecoder vtables, structs, the
// Wels* factory functions) — only this translation unit needs them; see
// openh264_codec.h for why they're forward-declared there instead.
#include "vendor/codec_api.h"

namespace reactor {
namespace {

// Matches the H264 SdpVideoFormat already advertised by the composite
// ReactorCompositeVideoEncoderFactory/ReactorCompositeVideoDecoderFactory in
// reactor_webrtc.cpp for the non-OpenH264 build, so a peer sees the same
// profile whether or not this process loaded OpenH264.
webrtc::SdpVideoFormat H264Format() {
  return webrtc::SdpVideoFormat("H264", {{"level-asymmetry-allowed", "1"},
                                          {"packetization-mode", "1"},
                                          {"profile-level-id", "42e01f"}});
}

#if defined(_WIN32)
void* LoadLib(const std::string& path) {
  // `path` is UTF-8 (it crossed the FFI boundary as a Rust `CString` built
  // from a UTF-8 `Path`). LoadLibraryA would decode these bytes using the
  // process's active ANSI code page instead of UTF-8, mangling any
  // non-ASCII byte (e.g. a LOCALAPPDATA path under a non-English username)
  // and silently failing to load — so widen to UTF-16 ourselves and use
  // LoadLibraryW instead.
  int wlen = MultiByteToWideChar(CP_UTF8, 0, path.c_str(), -1, nullptr, 0);
  if (wlen <= 0) return nullptr;
  std::vector<wchar_t> wpath(static_cast<size_t>(wlen));
  MultiByteToWideChar(CP_UTF8, 0, path.c_str(), -1, wpath.data(), wlen);
  return reinterpret_cast<void*>(LoadLibraryW(wpath.data()));
}
void* LookupSymbol(void* handle, const char* name) {
  return reinterpret_cast<void*>(
      GetProcAddress(reinterpret_cast<HMODULE>(handle), name));
}
void CloseLib(void* handle) {
  if (handle) FreeLibrary(reinterpret_cast<HMODULE>(handle));
}
#else
void* LoadLib(const std::string& path) {
  return dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
}
void* LookupSymbol(void* handle, const char* name) {
  return dlsym(handle, name);
}
void CloseLib(void* handle) {
  if (handle) dlclose(handle);
}
#endif

}  // namespace

// ── OpenH264Library ──────────────────────────────────────────────────────

std::unique_ptr<OpenH264Library> OpenH264Library::Open(const std::string& path) {
  // Can't use make_unique: the constructor is private (Open() is the only
  // way to build one, so an unloadable library never escapes as a "valid"
  // object with a null handle).
  std::unique_ptr<OpenH264Library> lib(new OpenH264Library());
  lib->handle_ = LoadLib(path);
  if (!lib->handle_) return lib;  // ok_ stays false

  lib->create_encoder_ = reinterpret_cast<int (*)(ISVCEncoder**)>(
      LookupSymbol(lib->handle_, "WelsCreateSVCEncoder"));
  lib->destroy_encoder_ = reinterpret_cast<void (*)(ISVCEncoder*)>(
      LookupSymbol(lib->handle_, "WelsDestroySVCEncoder"));
  lib->create_decoder_ = reinterpret_cast<long (*)(ISVCDecoder**)>(
      LookupSymbol(lib->handle_, "WelsCreateDecoder"));
  lib->destroy_decoder_ = reinterpret_cast<void (*)(ISVCDecoder*)>(
      LookupSymbol(lib->handle_, "WelsDestroyDecoder"));

  lib->ok_ = lib->create_encoder_ && lib->destroy_encoder_ &&
             lib->create_decoder_ && lib->destroy_decoder_;
  return lib;
}

OpenH264Library::~OpenH264Library() { CloseLib(handle_); }

ISVCEncoder* OpenH264Library::CreateEncoder() const {
  if (!ok_) return nullptr;
  ISVCEncoder* encoder = nullptr;
  if (create_encoder_(&encoder) != 0) return nullptr;
  return encoder;
}

void OpenH264Library::DestroyEncoder(ISVCEncoder* encoder) const {
  if (encoder && destroy_encoder_) destroy_encoder_(encoder);
}

ISVCDecoder* OpenH264Library::CreateDecoder() const {
  if (!ok_) return nullptr;
  ISVCDecoder* decoder = nullptr;
  if (create_decoder_(&decoder) != 0) return nullptr;
  return decoder;
}

void OpenH264Library::DestroyDecoder(ISVCDecoder* decoder) const {
  if (decoder && destroy_decoder_) destroy_decoder_(decoder);
}

namespace {

// ── Encoder ──────────────────────────────────────────────────────────────

class OpenH264VideoEncoder : public webrtc::VideoEncoder {
 public:
  explicit OpenH264VideoEncoder(std::shared_ptr<OpenH264Library> lib)
      : lib_(std::move(lib)) {}

  ~OpenH264VideoEncoder() override { Release(); }

  int InitEncode(const webrtc::VideoCodec* settings, const Settings&) override {
    if (!lib_ || !lib_->ok()) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    Release();
    encoder_ = lib_->CreateEncoder();
    if (!encoder_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;

    width_ = settings ? static_cast<int>(settings->width) : 0;
    height_ = settings ? static_cast<int>(settings->height) : 0;
    // VideoCodec::maxBitrate is kilobits/sec; OpenH264 wants bits/sec.
    int bitrate_bps =
        settings && settings->maxBitrate > 0
            ? static_cast<int>(settings->maxBitrate) * 1000
            : 500000;
    float frame_rate = settings && settings->maxFramerate > 0
                            ? static_cast<float>(settings->maxFramerate)
                            : 30.0f;

    SEncParamExt params;
    memset(&params, 0, sizeof(params));
    // GetDefaultParams fills sane defaults for every field we don't
    // explicitly set below (matches OpenH264's own "EncoderUsageExample2").
    encoder_->GetDefaultParams(&params);
    params.iUsageType = CAMERA_VIDEO_REAL_TIME;
    params.iPicWidth = width_;
    params.iPicHeight = height_;
    params.iTargetBitrate = bitrate_bps;
    params.iRCMode = RC_BITRATE_MODE;
    params.fMaxFrameRate = frame_rate;
    params.iTemporalLayerNum = 1;
    params.iSpatialLayerNum = 1;
    params.bEnableFrameSkip = false;
    params.iMultipleThreadIdc = 0;  // let the encoder choose
    params.sSpatialLayers[0].iVideoWidth = width_;
    params.sSpatialLayers[0].iVideoHeight = height_;
    params.sSpatialLayers[0].fFrameRate = frame_rate;
    params.sSpatialLayers[0].iSpatialBitrate = bitrate_bps;
    params.sSpatialLayers[0].uiProfileIdc = PRO_BASELINE;
    params.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;

    if (encoder_->InitializeExt(&params) != cmResultSuccess) {
      lib_->DestroyEncoder(encoder_);
      encoder_ = nullptr;
      return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    }

    int video_format = videoFormatI420;
    encoder_->SetOption(ENCODER_OPTION_DATAFORMAT, &video_format);
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t RegisterEncodeCompleteCallback(
      webrtc::EncodedImageCallback* cb) override {
    callback_ = cb;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Release() override {
    if (encoder_) {
      encoder_->Uninitialize();
      lib_->DestroyEncoder(encoder_);
      encoder_ = nullptr;
    }
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Encode(
      const webrtc::VideoFrame& frame,
      const std::vector<webrtc::VideoFrameType>* frame_types) override {
    if (!encoder_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;

    webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 =
        frame.video_frame_buffer()->ToI420();

    if (frame_types) {
      for (auto ft : *frame_types) {
        if (ft == webrtc::VideoFrameType::kVideoFrameKey) {
          encoder_->ForceIntraFrame(true);
          break;
        }
      }
    }

    SSourcePicture pic;
    memset(&pic, 0, sizeof(pic));
    pic.iColorFormat = videoFormatI420;
    pic.iPicWidth = i420->width();
    pic.iPicHeight = i420->height();
    pic.iStride[0] = i420->StrideY();
    pic.iStride[1] = i420->StrideU();
    pic.iStride[2] = i420->StrideV();
    pic.pData[0] = const_cast<uint8_t*>(i420->DataY());
    pic.pData[1] = const_cast<uint8_t*>(i420->DataU());
    pic.pData[2] = const_cast<uint8_t*>(i420->DataV());
    pic.uiTimeStamp = frame.rtp_timestamp();

    SFrameBSInfo info;
    memset(&info, 0, sizeof(info));
    if (encoder_->EncodeFrame(&pic, &info) != cmResultSuccess) {
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    if (info.eFrameType == videoFrameTypeSkip || info.iLayerNum == 0 ||
        !callback_) {
      return WEBRTC_VIDEO_CODEC_OK;
    }

    // Concatenate every layer's Annex-B bytes (SPS/PPS and slice NALs often
    // land in separate layers) into one contiguous bitstream — libwebrtc's
    // H264 RTP packetizer expects a single buffer per encoded image.
    size_t total = 0;
    for (int l = 0; l < info.iLayerNum; ++l) {
      const SLayerBSInfo& layer = info.sLayerInfo[l];
      for (int n = 0; n < layer.iNalCount; ++n) total += layer.pNalLengthInByte[n];
    }
    if (total == 0) return WEBRTC_VIDEO_CODEC_OK;

    // Concrete type (not EncodedImageBufferInterface): the interface only
    // declares the const `data()` overload, and we need the mutable one to
    // memcpy into.
    webrtc::scoped_refptr<webrtc::EncodedImageBuffer> buffer =
        webrtc::EncodedImageBuffer::Create(total);
    size_t offset = 0;
    for (int l = 0; l < info.iLayerNum; ++l) {
      const SLayerBSInfo& layer = info.sLayerInfo[l];
      size_t layer_len = 0;
      for (int n = 0; n < layer.iNalCount; ++n) layer_len += layer.pNalLengthInByte[n];
      memcpy(buffer->data() + offset, layer.pBsBuf, layer_len);
      offset += layer_len;
    }

    webrtc::EncodedImage img;
    img.SetEncodedData(buffer);
    img.SetFrameType(info.eFrameType == videoFrameTypeIDR
                          ? webrtc::VideoFrameType::kVideoFrameKey
                          : webrtc::VideoFrameType::kVideoFrameDelta);
    img._encodedWidth = static_cast<uint32_t>(width_);
    img._encodedHeight = static_cast<uint32_t>(height_);
    img.SetRtpTimestamp(frame.rtp_timestamp());

    webrtc::CodecSpecificInfo codec_info;
    codec_info.codecType = webrtc::kVideoCodecH264;
    codec_info.codecSpecific.H264.packetization_mode =
        webrtc::H264PacketizationMode::NonInterleaved;
    codec_info.codecSpecific.H264.temporal_idx = webrtc::kNoTemporalIdx;
    codec_info.codecSpecific.H264.idr_frame =
        (info.eFrameType == videoFrameTypeIDR);
    codec_info.codecSpecific.H264.base_layer_sync = false;

    callback_->OnEncodedImage(img, &codec_info);
    return WEBRTC_VIDEO_CODEC_OK;
  }

  void SetRates(const RateControlParameters& params) override {
    if (!encoder_) return;
    int bitrate_bps = static_cast<int>(params.bitrate.get_sum_bps());
    if (bitrate_bps > 0) {
      SBitrateInfo bitrate_info;
      bitrate_info.iLayer = SPATIAL_LAYER_ALL;
      bitrate_info.iBitrate = bitrate_bps;
      encoder_->SetOption(ENCODER_OPTION_BITRATE, &bitrate_info);
    }
    if (params.framerate_fps > 0) {
      float fps = static_cast<float>(params.framerate_fps);
      encoder_->SetOption(ENCODER_OPTION_FRAME_RATE, &fps);
    }
  }

  EncoderInfo GetEncoderInfo() const override {
    EncoderInfo info;
    info.implementation_name = "OpenH264";
    info.is_hardware_accelerated = false;
    return info;
  }

 private:
  std::shared_ptr<OpenH264Library> lib_;
  ISVCEncoder* encoder_ = nullptr;
  webrtc::EncodedImageCallback* callback_ = nullptr;
  int width_ = 0;
  int height_ = 0;
};

// ── Decoder ──────────────────────────────────────────────────────────────

class OpenH264VideoDecoder : public webrtc::VideoDecoder {
 public:
  explicit OpenH264VideoDecoder(std::shared_ptr<OpenH264Library> lib)
      : lib_(std::move(lib)) {}

  ~OpenH264VideoDecoder() override { Release(); }

  bool Configure(const Settings&) override {
    if (!lib_ || !lib_->ok()) return false;
    Release();
    decoder_ = lib_->CreateDecoder();
    if (!decoder_) return false;

    SDecodingParam params;
    memset(&params, 0, sizeof(params));
    params.sVideoProperty.size = sizeof(SVideoProperty);
    // WebRTC negotiates plain AVC (not the SVC extension) for "H264".
    params.sVideoProperty.eVideoBsType = VIDEO_BITSTREAM_AVC;
    if (decoder_->Initialize(&params) != cmResultSuccess) {
      lib_->DestroyDecoder(decoder_);
      decoder_ = nullptr;
      return false;
    }
    return true;
  }

  int32_t RegisterDecodeCompleteCallback(
      webrtc::DecodedImageCallback* cb) override {
    callback_ = cb;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Release() override {
    if (decoder_) {
      decoder_->Uninitialize();
      lib_->DestroyDecoder(decoder_);
      decoder_ = nullptr;
    }
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Decode(const webrtc::EncodedImage& input,
                 int64_t /*render_time_ms*/) override {
    if (!decoder_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;

    unsigned char* planes[3] = {nullptr, nullptr, nullptr};
    SBufferInfo buf_info;
    memset(&buf_info, 0, sizeof(buf_info));

    DECODING_STATE state = decoder_->DecodeFrameNoDelay(
        input.data(), static_cast<int>(input.size()), planes, &buf_info);
    // dsFramePending just means "no picture yet, keep feeding data" — not
    // an error (e.g. right after an IDR before enough slices arrived).
    if (state != dsErrorFree && state != dsFramePending) {
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    if (buf_info.iBufferStatus != 1 || !callback_) {
      return WEBRTC_VIDEO_CODEC_NO_OUTPUT;
    }

    const SSysMEMBuffer& sys = buf_info.UsrData.sSystemBuffer;
    webrtc::scoped_refptr<webrtc::I420Buffer> out = webrtc::I420Buffer::Copy(
        sys.iWidth, sys.iHeight, planes[0], sys.iStride[0], planes[1],
        sys.iStride[1], planes[2], sys.iStride[1]);

    webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
                                    .set_video_frame_buffer(out)
                                    .set_rtp_timestamp(input.RtpTimestamp())
                                    .set_timestamp_us(0)
                                    .build();
    callback_->Decoded(frame);
    return WEBRTC_VIDEO_CODEC_OK;
  }

  DecoderInfo GetDecoderInfo() const override {
    return {"OpenH264", /*is_hardware_accelerated=*/false};
  }

 private:
  std::shared_ptr<OpenH264Library> lib_;
  ISVCDecoder* decoder_ = nullptr;
  webrtc::DecodedImageCallback* callback_ = nullptr;
};

// ── Factories ────────────────────────────────────────────────────────────

class OpenH264VideoEncoderFactory : public webrtc::VideoEncoderFactory {
 public:
  explicit OpenH264VideoEncoderFactory(std::shared_ptr<OpenH264Library> lib)
      : lib_(std::move(lib)),
        builtin_(webrtc::CreateBuiltinVideoEncoderFactory()) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    auto formats = builtin_->GetSupportedFormats();
    if (lib_ && lib_->ok()) formats.push_back(H264Format());
    return formats;
  }

  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (format.name == "H264" && lib_ && lib_->ok()) {
      return std::make_unique<OpenH264VideoEncoder>(lib_);
    }
    return builtin_->Create(env, format);
  }

 private:
  std::shared_ptr<OpenH264Library> lib_;
  std::unique_ptr<webrtc::VideoEncoderFactory> builtin_;
};

class OpenH264VideoDecoderFactory : public webrtc::VideoDecoderFactory {
 public:
  explicit OpenH264VideoDecoderFactory(std::shared_ptr<OpenH264Library> lib)
      : lib_(std::move(lib)),
        builtin_(webrtc::CreateBuiltinVideoDecoderFactory()) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    auto formats = builtin_->GetSupportedFormats();
    if (lib_ && lib_->ok()) formats.push_back(H264Format());
    return formats;
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (format.name == "H264" && lib_ && lib_->ok()) {
      return std::make_unique<OpenH264VideoDecoder>(lib_);
    }
    return builtin_->Create(env, format);
  }

 private:
  std::shared_ptr<OpenH264Library> lib_;
  std::unique_ptr<webrtc::VideoDecoderFactory> builtin_;
};

}  // namespace

std::unique_ptr<webrtc::VideoEncoderFactory> CreateOpenH264VideoEncoderFactory(
    std::shared_ptr<OpenH264Library> lib) {
  return std::make_unique<OpenH264VideoEncoderFactory>(std::move(lib));
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateOpenH264VideoDecoderFactory(
    std::shared_ptr<OpenH264Library> lib) {
  return std::make_unique<OpenH264VideoDecoderFactory>(std::move(lib));
}

}  // namespace reactor
