// NV12 (BT.601 limited range) to packed R,G,B bytes, one thread per pixel.
// Chroma is shared by each 2x2 block, as libyuv's NV12/I420 to RGB does.
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
