// H.264 decode on NVIDIA GPUs with NVDEC (the NVIDIA Video Codec SDK's
// nvcuvid). The CUDA driver and nvcuvid are loaded at run time from the
// libraries the NVIDIA driver installs (libcuda.so.1, libnvcuvid.so.1), through
// the vendored nv-codec-headers loader: nothing is linked at build time, and on
// a machine without an NVIDIA GPU or driver the library does not open and the
// backend is simply not offered. See vendor/README.md.
#pragma once

#include <memory>
#include <string>

#include "api/video_codecs/video_decoder_factory.h"

namespace reactor {

// The loaded CUDA driver and nvcuvid entry points, one GPU's primary context,
// and whether that GPU decodes 8-bit 4:2:0 H.264. Shared by every decoder of a
// factory, so the libraries and the context outlive them all.
class NvdecLibrary;

// Opens the driver libraries, initialises CUDA, retains the primary context of
// GPU `device` (an ordinal after CUDA_VISIBLE_DEVICES) and checks that it
// decodes H.264. Null, with the reason in `why` when not null, on any failure:
// no driver, no GPU, no such device, or no H.264 decode.
std::shared_ptr<NvdecLibrary> OpenNvdec(int device, std::string* why);

// H.264 decoders on `lib`. Lists the H.264 profiles NVDEC decodes and nothing
// else, so it is merged with the other factories, ahead of the software ones.
std::unique_ptr<webrtc::VideoDecoderFactory> CreateNvdecVideoDecoderFactory(
    std::shared_ptr<NvdecLibrary> lib);

}  // namespace reactor
