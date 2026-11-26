// Video decode on NVIDIA GPUs with NVDEC (the NVIDIA Video Codec SDK's
// nvcuvid): H.264, H.265 (Main), VP8, VP9 (profile 0) and AV1 (main), 8-bit
// 4:2:0, each where the GPU decodes it. The CUDA driver and nvcuvid are loaded at run time
// from the libraries the NVIDIA driver installs (libcuda.so.1,
// libnvcuvid.so.1), through the vendored nv-codec-headers loader: nothing is
// linked at build time, and on a machine without an NVIDIA GPU or driver the
// library does not open and the backend is simply not offered. See
// vendor/README.md.
#pragma once

#include <memory>
#include <string>

#include "api/video/video_codec_type.h"
#include "api/video_codecs/video_decoder_factory.h"

namespace reactor {

// The loaded CUDA driver and nvcuvid entry points, one GPU's primary context,
// and which codecs that GPU decodes. Shared by every decoder of a factory, so
// the libraries and the context outlive them all.
class NvdecLibrary;

// Opens the driver libraries, initialises CUDA, retains the primary context of
// GPU `device` (an ordinal after CUDA_VISIBLE_DEVICES) and asks it which of
// H.264, H.265, VP8, VP9 and AV1 it decodes. Null, with the reason in `why` when not
// null, on any failure: no driver, no GPU, no such device, or none of them.
std::shared_ptr<NvdecLibrary> OpenNvdec(int device, std::string* why);

// Whether `lib`'s GPU decodes `codec` (8-bit 4:2:0; Main for H.265, profile
// 0 for VP9, main for AV1).
bool NvdecDecodes(const NvdecLibrary& lib, webrtc::VideoCodecType codec);

// Decoders on `lib` for every codec its GPU decodes. Lists those codecs and
// nothing else, so it is merged with the other factories, ahead of the
// software ones.
std::unique_ptr<webrtc::VideoDecoderFactory> CreateNvdecVideoDecoderFactory(
    std::shared_ptr<NvdecLibrary> lib);

}  // namespace reactor
