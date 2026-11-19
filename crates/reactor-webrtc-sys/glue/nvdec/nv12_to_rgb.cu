// NV12 to packed R,G,B bytes on the GPU, for NVDEC's pictures: BT.601 limited
// range, the conversion libyuv's I420ToRAW does, with each 2x2 block of
// pixels sharing one chroma sample. One thread per pixel.
//
// The build does not compile this file: nv12_to_rgb_ptx.h holds its PTX, which
// the driver compiles for the GPU at cuModuleLoadData, so building needs no
// NVIDIA toolkit. After changing it, regenerate that header with
//
//   nvcc -ptx -O3 -arch=compute_80 nv12_to_rgb.cu -o nv12_to_rgb.ptx
//
// and paste the PTX into it, lowering its `.version` line to 7.0. nvcc writes
// its own toolkit's PTX ISA version there, and a driver older than that
// version fails the load; 7.0 (CUDA 11.0) is the first with sm_80, and the
// kernel uses nothing newer, so CUDA 11.0 and later drivers load it (keep it
// that way when changing it). compute_80 PTX runs on every GPU NVDEC
// serves here (Ampere and later); a driver that fails the load leaves NVDEC
// handing out NV12 pictures for the CPU to convert.

__device__ __forceinline__ unsigned char sat(float v) {
  return (unsigned char)fminf(fmaxf(v + 0.5f, 0.f), 255.f);
}

extern "C" __global__ void nv12_to_rgb24(const unsigned char* __restrict__ y_plane,
                                         unsigned int y_pitch,
                                         const unsigned char* __restrict__ uv_plane,
                                         unsigned int uv_pitch,
                                         unsigned char* __restrict__ rgb,
                                         unsigned int rgb_pitch, int width,
                                         int height) {
  const int x = blockIdx.x * blockDim.x + threadIdx.x;
  const int y = blockIdx.y * blockDim.y + threadIdx.y;
  if (x >= width || y >= height) return;
  const float luma = 1.164383f * ((float)y_plane[y * y_pitch + x] - 16.f);
  const unsigned char* c = uv_plane + (y >> 1) * uv_pitch + (x & ~1);
  const float u = (float)c[0] - 128.f;
  const float v = (float)c[1] - 128.f;
  unsigned char* o = rgb + y * rgb_pitch + x * 3;
  o[0] = sat(luma + 1.596027f * v);
  o[1] = sat(luma - 0.391762f * u - 0.812968f * v);
  o[2] = sat(luma + 2.017232f * u);
}
