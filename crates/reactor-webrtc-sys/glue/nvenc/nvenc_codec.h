// Video encode on NVIDIA GPUs with NVENC (the NVIDIA Video Codec SDK's
// nvEncodeAPI): H.264 (constrained baseline) and AV1 (main), 8-bit 4:2:0, each
// where the GPU encodes it. VP8 and VP9 have no NVENC encoder. The CUDA driver
// and NVENC are loaded at run time from the libraries the NVIDIA driver
// installs (libcuda.so.1, libnvidia-encode.so.1), through the nv-codec-headers
// loader vendored in ../nvdec/vendor: nothing is linked at build time, and on
// a machine without an NVIDIA GPU, driver or encoder the library does not open
// and the backend is simply not offered.
#pragma once

#include <memory>
#include <string>

#include "api/video/video_codec_type.h"
#include "api/video_codecs/video_encoder_factory.h"

namespace reactor {

// The loaded CUDA driver and NVENC entry points, one GPU's primary context,
// and which codecs that GPU encodes. Shared by every encoder of a factory, so
// the libraries and the context outlive them all.
class NvencLibrary;

// Opens the driver libraries, initialises CUDA, retains the primary context of
// GPU `device` (an ordinal after CUDA_VISIBLE_DEVICES) and asks it which of
// H.264 and AV1 it encodes. Null, with the reason in `why` when not null, on
// any failure: no driver, no GPU, no such device, no encoder on it (an A100,
// H100 or B200 has none), or none of them.
std::shared_ptr<NvencLibrary> OpenNvenc(int device, std::string* why);

// Whether `lib`'s GPU encodes `codec` (8-bit 4:2:0; constrained baseline for
// H.264, main for AV1).
bool NvencEncodes(const NvencLibrary& lib, webrtc::VideoCodecType codec);

// Encoders on `lib` for every codec its GPU encodes. Lists those codecs and
// nothing else, so it is merged with the other factories, ahead of the
// software ones.
std::unique_ptr<webrtc::VideoEncoderFactory> CreateNvencVideoEncoderFactory(
    std::shared_ptr<NvencLibrary> lib);

}  // namespace reactor
