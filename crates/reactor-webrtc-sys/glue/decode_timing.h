// Temporary decode-stage timing for profiling (not for merge).
// Set REACTOR_WEBRTC_DECODE_TIMING=1 to record; stages print to stderr at exit
// (median / p95 / mean in ms, the first kWarmup samples of each stage skipped).
#pragma once

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <map>
#include <mutex>
#include <string>
#include <vector>

namespace reactor_decode_timing {

constexpr size_t kWarmup = 10;

inline bool Enabled() {
  static const bool on = std::getenv("REACTOR_WEBRTC_DECODE_TIMING") != nullptr;
  return on;
}

struct Store {
  std::mutex mu;
  std::map<std::string, std::vector<double>> samples;  // ms

  ~Store() {
    std::lock_guard<std::mutex> lock(mu);
    for (auto& [stage, xs] : samples) {
      if (xs.size() <= kWarmup) continue;
      std::vector<double> v(xs.begin() + kWarmup, xs.end());
      std::sort(v.begin(), v.end());
      double sum = 0;
      for (double x : v) sum += x;
      const double median = v[v.size() / 2];
      const double p95 = v[std::min(v.size() - 1, v.size() * 95 / 100)];
      std::fprintf(stderr,
                   "[decode-timing] %-22s n=%-5zu median=%.3f p95=%.3f "
                   "mean=%.3f ms\n",
                   stage.c_str(), v.size(), median, p95, sum / v.size());
    }
    std::fflush(stderr);
  }
};

inline Store& GetStore() {
  static Store store;
  return store;
}

inline void Record(const char* stage, double ms) {
  Store& s = GetStore();
  std::lock_guard<std::mutex> lock(s.mu);
  s.samples[stage].push_back(ms);
}

class Scope {
 public:
  explicit Scope(const char* stage) : stage_(stage), on_(Enabled()) {
    if (on_) t0_ = std::chrono::steady_clock::now();
  }
  ~Scope() {
    if (!on_) return;
    const auto t1 = std::chrono::steady_clock::now();
    Record(stage_, std::chrono::duration<double, std::milli>(t1 - t0_).count());
  }
  Scope(const Scope&) = delete;
  Scope& operator=(const Scope&) = delete;

 private:
  const char* stage_;
  bool on_;
  std::chrono::steady_clock::time_point t0_;
};

}  // namespace reactor_decode_timing

#define DECODE_TIMING_CAT2(a, b) a##b
#define DECODE_TIMING_CAT(a, b) DECODE_TIMING_CAT2(a, b)
#define DECODE_TIMING(stage) \
  ::reactor_decode_timing::Scope DECODE_TIMING_CAT(decode_timing_, __LINE__)(stage)
