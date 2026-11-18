# Vendored NVIDIA Video Codec SDK headers

`dynlink_cuda.h`, `dynlink_cuviddec.h`, `dynlink_nvcuvid.h`,
`dynlink_loader.h` and `nvEncodeAPI.h`, copied **verbatim, unmodified** from
[FFmpeg/nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers) tag
`n12.2.72.0` (`include/ffnvcodec/`). MIT, with the license text in each file's
header comment, kept as it requires.

## Why these and not the SDK

They declare the CUDA driver API and NVDEC (`nvcuvid`) entry points as
function-pointer types, and `dynlink_loader.h` resolves them at run time with
`dlopen` from the libraries the NVIDIA driver installs (`libcuda.so.1`,
`libnvcuvid.so.1`). Nothing is linked at build time and no NVIDIA toolkit is
needed to build, so the same library runs on a machine with no NVIDIA GPU,
where the loader fails and the NVDEC backend is not offered. This is how FFmpeg
uses the same headers.

`nvEncodeAPI.h` is here only because `dynlink_loader.h` includes it; NVENC is
not used.

To update: copy the five files from a newer tag, unmodified, and update the tag
above.
