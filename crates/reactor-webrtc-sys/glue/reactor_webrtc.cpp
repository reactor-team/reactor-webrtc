// reactor-webrtc glue — the C++ side of the FFI boundary.
//
// M1: the first real objects backed by our owned libwebrtc.a — the builtin
// codec factories, a real PeerConnectionFactory (threads + media engine), and
// PeerConnections with a callback bridge (PeerConnectionObserver +
// CreateSessionDescriptionObserver forwarded to C function pointers).
//
// The builtin codec factories live in their own gn targets that the `webrtc`
// umbrella normally omits; webrtc-build/patches/0001-* injects them into the
// umbrella's deps so these symbols resolve in libwebrtc.a.
//
// The remaining surface in `src/lib.rs` (tracks, frame I/O, ADM, set-remote-
// description / add-ice-candidate) lands next and will eventually be generated
// rather than hand-written.
//
// Android bootstrap (reactor_webrtc_android_init / _init_context): located at
// the bottom of this file, guarded by #ifdef WEBRTC_ANDROID.

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <limits>

#include "api/stats/rtc_stats_collector_callback.h"
#include "api/stats/rtc_stats_report.h"
#include "api/stats/rtcstats_objects.h"
#include "api/audio/audio_device.h"
#include "api/audio/audio_device_defines.h"
#include "api/audio/audio_processing.h"
#include "api/audio/builtin_audio_processing_builder.h"
#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/audio_options.h"
#include "api/create_peerconnection_factory.h"
#include "api/data_channel_interface.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/frame_transformer_interface.h"
#include "api/jsep.h"
#include "api/make_ref_counted.h"
#include "api/media_stream_interface.h"
#include "api/media_types.h"
#include "api/peer_connection_interface.h"
#include "api/rtc_error.h"
#include "api/rtp_receiver_interface.h"
#include "api/rtp_sender_interface.h"
#include "api/rtp_transceiver_direction.h"
#include "api/rtp_transceiver_interface.h"
#include "api/scoped_refptr.h"
#include "api/set_local_description_observer_interface.h"
#include "api/set_remote_description_observer_interface.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "api/video/video_sink_interface.h"
#include "api/video/video_source_interface.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "media/base/video_broadcaster.h"
#include "modules/audio_device/include/audio_device_default.h"
#include "modules/video_coding/codecs/interface/common_constants.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "pc/video_track_source.h"
#include "rtc_base/copy_on_write_buffer.h"
#include "rtc_base/thread.h"
#include "rtc_base/time_utils.h"
#include "third_party/libyuv/include/libyuv/convert.h"
#include "third_party/libyuv/include/libyuv/convert_argb.h"

// ── C ABI types (must match crates/reactor-webrtc-sys/src/lib.rs) ─────────────

extern "C" {
// A single entry in the stats snapshot delivered to
// reactor_webrtc_peer_connection_get_stats. `kind` discriminates which set of
// fields is populated; unused fields are zero-initialised.
//   kind 0 = inbound_rtp   (RTCInboundRtpStreamStats)
//   kind 1 = outbound_rtp  (RTCOutboundRtpStreamStats)
//   kind 2 = candidate_pair (RTCIceCandidatePairStats)
// Fields are ordered to avoid padding; the layout must match the repr(C)
// struct in reactor-webrtc-sys/src/lib.rs.
struct ReactorStatEntry {
  int32_t  kind;
  uint32_t ssrc;
  // 4-byte integer fields
  uint32_t packets_received;
  int32_t  packets_lost;
  uint32_t nack_count;
  uint32_t packets_sent;
  int32_t  pair_state;  // 0=waiting 1=in_progress 2=failed 3=succeeded 4=cancelled
  uint32_t retransmitted_packets_sent;
  // 8-byte fields (natural alignment)
  uint64_t bytes_received;
  uint64_t bytes_sent;
  uint64_t priority;
  double   jitter;                  // seconds
  double   total_decode_time;       // seconds
  double   target_bitrate;          // bps
  double   round_trip_time;         // seconds, 0 if not measured
  double   current_round_trip_time; // seconds, 0 if not measured
};

// PeerConnectionObserver events, forwarded to the safe crate. Any pointer may
// be null (the field is `Option<extern "C" fn>` on the Rust side).
struct ReactorPcCallbacks {
  void* userdata;
  void (*on_signaling_change)(void* userdata, int state);
  void (*on_connection_change)(void* userdata, int state);
  void (*on_ice_gathering_change)(void* userdata, int state);
  void (*on_ice_candidate)(void* userdata, const char* sdp_mid,
                           int sdp_mline_index, const char* candidate);
  void (*on_data_channel)(void* userdata, void* data_channel);
  void (*on_renegotiation_needed)(void* userdata);
  // A remote track was added; `track` is an owned MediaStreamTrack handle the
  // receiver must free with reactor_webrtc_media_stream_track_destroy.
  void (*on_track)(void* userdata, void* track);
};

// A single ICE (STUN/TURN) server. `urls` holds `urls_len` NUL-terminated
// strings that share the credentials in this entry. `username` and `password`
// may be null, which reads as an empty credential. Every pointer is borrowed
// for the duration of reactor_webrtc_peer_connection_create.
struct ReactorIceServer {
  const char* const* urls;
  size_t             urls_len;
  const char*        username;
  const char*        password;
};

// Peer-connection configuration. Each entry of `servers` keeps its own
// credentials, so several TURN servers with different credentials stay
// distinct. The policy fields use an explicit integer encoding, independent of
// the webrtc:: enum order:
//   ice_transport_type:         0=all 1=relay 2=no-host 3=none
//   continual_gathering_policy: 0=gather-once 1=gather-continually
//   bundle_policy:              0=balanced 1=max-bundle 2=max-compat
//   tcp_candidate_policy:       0=disabled 1=enabled
// An unknown value falls back to 0.
struct ReactorRtcConfig {
  const ReactorIceServer* servers;
  size_t                  servers_len;
  int                     ice_transport_type;
  int                     continual_gathering_policy;
  int                     min_port;  // 0 = not specified
  int                     max_port;  // 0 = not specified
  int                     bundle_policy;
  // Milliseconds. <=0 keeps the libwebrtc default (~30 s in practice).
  int                     ice_connection_receiving_timeout_ms;
  // ICE check interval on a well-connected path in ms. <=0 = libwebrtc default.
  int                     ice_check_interval_strong_connectivity_ms;
  int                     tcp_candidate_policy;
};
}  // extern "C"

namespace {

// The MediaStream every local track we publish belongs to, signalled as the
// msid stream id. The remote libwebrtc derives each receive stream's sync group
// from that id: streams sharing one id have their audio and video aligned
// against each other's RTCP sender reports, while a track published with no
// stream ("a=msid:-") is played out as it arrives. Publishing every sender
// under one id is what keeps an audio track in sync with its video.
constexpr const char* kReactorStreamId = "reactor-stream";

// Opt-in audio path tracing (REACTOR_WEBRTC_AUDIO_DEBUG=1) → stderr. Off by
// default. Used to pinpoint capture (push) vs playout-pump vs sink delivery.
bool audio_debug() {
  static const bool on = std::getenv("REACTOR_WEBRTC_AUDIO_DEBUG") != nullptr;
  return on;
}

// A video source we can push externally-produced frames into. VideoBroadcaster
// fans frames out to the connected encoder sink(s).
class FrameSource : public webrtc::VideoTrackSource {
 public:
  FrameSource() : webrtc::VideoTrackSource(/*remote=*/false) {}
  void PushFrame(const webrtc::VideoFrame& frame) { broadcaster_.OnFrame(frame); }

 protected:
  webrtc::VideoSourceInterface<webrtc::VideoFrame>* source() override {
    return &broadcaster_;
  }

 private:
  webrtc::VideoBroadcaster broadcaster_;
};

// A custom AudioDeviceModule we push PCM into (capture) and which pumps the
// receive path (playout) so remote audio-track sinks fire — no real audio
// hardware. AudioDeviceModuleDefault stubs the ~40 ADM methods; we override the
// capture/playout bits.
class FrameAdm : public webrtc::webrtc_impl::AudioDeviceModuleDefault<
                     webrtc::AudioDeviceModule> {
 public:
  ~FrameAdm() override { StopPlayout(); }

  int32_t RegisterAudioCallback(webrtc::AudioTransport* transport) override {
    std::lock_guard<std::mutex> lock(mutex_);
    transport_ = transport;
    if (audio_debug())
      fprintf(stderr, "[reactor-webrtc] ADM RegisterAudioCallback transport=%p\n",
              static_cast<void*>(transport));
    return 0;
  }
  bool RecordingIsInitialized() const override { return true; }
  bool Recording() const override { return true; }

  // Gate the playout pump (e.g. to stay fully silent in send-only / headless
  // scenarios). Enabled by default.
  void SetPlayoutEnabled(bool enabled) { playout_enabled_.store(enabled); }

  // Playout pump: pulls (and discards) 10ms render blocks so the receive
  // pipeline runs and remote audio-track sinks are invoked.
  int32_t StartPlayout() override {
    if (audio_debug())
      fprintf(stderr, "[reactor-webrtc] ADM StartPlayout (enabled=%d already=%d)\n",
              playout_enabled_.load(), playing_.load());
    if (!playout_enabled_.load()) return 0;
    if (playing_.exchange(true)) return 0;
    play_thread_ = std::thread([this] { PlayoutLoop(); });
    return 0;
  }
  int32_t StopPlayout() override {
    if (!playing_.exchange(false)) return 0;
    if (play_thread_.joinable()) play_thread_.join();
    return 0;
  }
  bool Playing() const override { return playing_.load(); }

  // Deliver interleaved int16 PCM (samples_per_channel frames) to the engine.
  void PushPcm(const int16_t* pcm, size_t samples_per_channel,
               uint32_t sample_rate, size_t channels) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!transport_ || !pcm || channels == 0) return;
    if (audio_debug()) {
      static uint64_t n = 0;
      if (++n % 500 == 1)
        fprintf(stderr, "[reactor-webrtc] ADM PushPcm #%llu (%zu frames %u Hz %zu ch)\n",
                (unsigned long long)n, samples_per_channel, sample_rate, channels);
    }
    uint32_t new_mic_level = 0;
    transport_->RecordedDataIsAvailable(
        pcm, samples_per_channel, sizeof(int16_t) * channels, channels,
        sample_rate, /*totalDelayMS=*/0, /*clockDrift=*/0, /*currentMicLevel=*/0,
        /*keyPressed=*/false, new_mic_level);
  }

 private:
  void PlayoutLoop() {
    const uint32_t rate = 48000;
    const size_t channels = 2;
    const size_t frames = rate / 100;  // 10ms
    const auto period = std::chrono::milliseconds(10);
    std::vector<int16_t> scratch(frames * channels);
    uint64_t pulls = 0, produced = 0;

    // This pump is the clock for the whole receive path: libwebrtc feeds the
    // sinks of remote audio tracks from the render pull, so its rate is the
    // rate at which received audio reaches the application. It therefore runs
    // on absolute deadlines. Sleeping for a fixed period *after* the pull would
    // add the cost of every pull to every period, and the arrears compound into
    // a permanently slow clock, starving every consumer downstream.
    auto next = std::chrono::steady_clock::now();

    while (playing_.load()) {
      size_t out = 0;
      bool pulled = false;
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (transport_) {
          int64_t elapsed = 0, ntp = 0;
          transport_->NeedMorePlayData(frames, sizeof(int16_t) * channels,
                                       channels, rate, scratch.data(), out,
                                       &elapsed, &ntp);
          pulled = true;
        }
      }

      // scratch belongs to this thread, so the peak scan stays outside the lock
      // and cannot delay a concurrent push into the device.
      if (pulled) {
        // out is just the requested frame count echoed back; measure the
        // mixed peak to tell real incoming audio from silence.
        int16_t peak = 0;
        for (size_t i = 0; i < out * channels && i < scratch.size(); ++i) {
          int16_t v = scratch[i] < 0 ? -scratch[i] : scratch[i];
          if (v > peak) peak = v;
        }
        if (peak > 0) ++produced;
        if (audio_debug() && ++pulls % 200 == 1)
          fprintf(stderr,
                  "[reactor-webrtc] ADM playout pump: %llu pulls, %llu non-silent, "
                  "last peak=%d (out=%zu)\n",
                  (unsigned long long)pulls, (unsigned long long)produced, peak, out);
      }

      next += period;
      const auto now = std::chrono::steady_clock::now();
      // A long stall (host suspend, scheduler starvation) is dropped rather
      // than repaid as a burst of back-to-back pulls, which would flood the
      // receive path with a spike of catch-up audio.
      if (next < now) next = now;
      std::this_thread::sleep_until(next);
    }
  }

  std::mutex mutex_;
  webrtc::AudioTransport* transport_ = nullptr;
  std::atomic<bool> playing_{false};
  std::atomic<bool> playout_enabled_{true};
  std::thread play_thread_;
};

// Bridges decoded frames from a (remote) audio track to a C callback. WebRTC
// audio-sink data is interleaved int16 PCM (bits_per_sample == 16).
class AudioFrameSink : public webrtc::AudioTrackSinkInterface {
 public:
  AudioFrameSink(void* userdata,
                 void (*on_audio)(void*, const int16_t*, int, int, int))
      : userdata_(userdata), on_audio_(on_audio) {}
  void OnData(const void* audio_data, int /*bits_per_sample*/, int sample_rate,
              size_t number_of_channels, size_t number_of_frames) override {
    // The playout pump fires this sink every 10 ms for every PeerConnection,
    // even for peers that are not sending audio.  When no RTP has arrived the
    // jitter buffer outputs all-zero frames (comfort noise / empty buffer).
    // Forwarding those frames to the model as if they were real audio doubles
    // (or multiplies) the effective audio input rate in multi-peer sessions,
    // causing the model to emit audio faster than peers can play it back.
    const int16_t* pcm = static_cast<const int16_t*>(audio_data);
    const size_t total = number_of_frames * number_of_channels;
    bool has_signal = false;
    for (size_t i = 0; i < total && !has_signal; ++i)
      if (pcm[i] != 0) has_signal = true;
    if (!has_signal) return;

    if (audio_debug()) {
      static uint64_t n = 0;
      if (++n % 100 == 1)
        fprintf(stderr, "[reactor-webrtc] audio sink OnData #%llu (%d Hz %zu ch %zu frames)\n",
                (unsigned long long)n, sample_rate, number_of_channels, number_of_frames);
    }
    if (on_audio_)
      on_audio_(userdata_, pcm, sample_rate,
                static_cast<int>(number_of_channels),
                static_cast<int>(number_of_frames));
  }

 private:
  void* userdata_;
  void (*on_audio_)(void*, const int16_t*, int, int, int);
};

// Bridges decoded frames from a (remote) video track to a C callback,
// converting to BGRA (width*height*4) on the way out.
class FrameSink : public webrtc::VideoSinkInterface<webrtc::VideoFrame> {
 public:
  FrameSink(void* userdata, void (*on_frame)(void*, const uint8_t*, int, int))
      : userdata_(userdata), on_frame_(on_frame) {}
  void OnFrame(const webrtc::VideoFrame& frame) override {
    if (!on_frame_) return;
    webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 =
        frame.video_frame_buffer()->ToI420();
    if (!i420) return;
    const int w = i420->width(), h = i420->height();
    bgra_.resize(static_cast<size_t>(w) * h * 4);
    // libyuv "ARGB" is B,G,R,A in memory == our BGRA byte order.
    libyuv::I420ToARGB(i420->DataY(), i420->StrideY(), i420->DataU(),
                       i420->StrideU(), i420->DataV(), i420->StrideV(),
                       bgra_.data(), w * 4, w, h);
    on_frame_(userdata_, bgra_.data(), w, h);
  }

 private:
  void* userdata_;
  void (*on_frame_)(void*, const uint8_t*, int, int);
  std::vector<uint8_t> bgra_;
};

// A per-track audio source that bypasses the shared ADM.  Each instance
// maintains the sinks registered by the VoiceEngine send channel for a
// specific peer connection and delivers PCM to those sinks directly, so
// different audio can be routed to different peers independently.
//
// PushPcm() is called from each peer's dedicated _audio_feed_loop Python
// thread.  Because each LocalAudioSource is owned by exactly one peer and
// its _audio_feed_loop is the only caller, the ChannelSend's serialised-call
// requirement is satisfied by construction — no PostTask to the shared worker
// thread is needed.  Routing through the worker thread adds latency and
// contention when multiple peers are active; calling directly keeps each
// peer's audio path independent and low-latency.
class LocalAudioSource
    : public webrtc::Notifier<webrtc::AudioSourceInterface> {
 public:
  static webrtc::scoped_refptr<LocalAudioSource> Create() {
    return webrtc::make_ref_counted<LocalAudioSource>();
  }

  webrtc::MediaSourceInterface::SourceState state() const override {
    return kLive;
  }
  bool remote() const override { return false; }

  void AddSink(webrtc::AudioTrackSinkInterface* sink) override {
    std::lock_guard<std::mutex> lock(sinks_mutex_);
    sinks_.push_back(sink);
  }
  void RemoveSink(webrtc::AudioTrackSinkInterface* sink) override {
    std::lock_guard<std::mutex> lock(sinks_mutex_);
    sinks_.erase(std::remove(sinks_.begin(), sinks_.end(), sink), sinks_.end());
  }

  // Deliver PCM directly to the registered sinks on the calling thread.
  void PushPcm(const int16_t* pcm, int samples_per_channel,
               int sample_rate, int channels) {
    std::lock_guard<std::mutex> lock(sinks_mutex_);
    for (auto* sink : sinks_) {
      sink->OnData(pcm, /*bits_per_sample=*/16, sample_rate,
                   static_cast<size_t>(channels),
                   static_cast<size_t>(samples_per_channel));
    }
  }

 private:
  std::mutex sinks_mutex_;
  std::vector<webrtc::AudioTrackSinkInterface*> sinks_;
};

// A track handle (the `MediaStreamTrack` in the Rust API). For a local track
// `source` or `audio_source` is set and frames can be pushed; for a remote
// track (from OnTrack) both sources are null and `sink`/`audio_sink` are used.
struct ReactorMediaStreamTrack {
  webrtc::scoped_refptr<FrameSource> source;              // local video
  webrtc::scoped_refptr<LocalAudioSource> audio_source;  // local audio (per-track)
  webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track;
  std::unique_ptr<FrameSink> sink;
  std::unique_ptr<AudioFrameSink> audio_sink;
};

// Bridges data-channel events to C callbacks.
// State int values: 0=Connecting 1=Open 2=Closing 3=Closed.
class ReactorDcObserver : public webrtc::DataChannelObserver {
 public:
  ReactorDcObserver(webrtc::DataChannelInterface* channel, void* userdata,
                    void (*on_message)(void*, const uint8_t*, size_t, int),
                    void (*on_state_change)(void*, int),
                    void (*on_buffered_amount_low)(void*),
                    uint64_t low_threshold)
      : channel_(channel),
        userdata_(userdata),
        on_message_(on_message),
        on_state_change_(on_state_change),
        on_buffered_amount_low_(on_buffered_amount_low),
        low_threshold_(low_threshold) {}

  void SetLowThreshold(uint64_t t) { low_threshold_ = t; }

  void OnStateChange() override {
    if (!on_state_change_) return;
    int s;
    switch (channel_->state()) {
      case webrtc::DataChannelInterface::kConnecting: s = 0; break;
      case webrtc::DataChannelInterface::kOpen:       s = 1; break;
      case webrtc::DataChannelInterface::kClosing:    s = 2; break;
      default:                                        s = 3; break;
    }
    on_state_change_(userdata_, s);
  }
  void OnMessage(const webrtc::DataBuffer& buffer) override {
    if (on_message_)
      on_message_(userdata_, buffer.data.cdata(), buffer.data.size(),
                  buffer.binary ? 1 : 0);
  }
  // M7907 removed buffered_amount_low_threshold() from the public API;
  // we track the threshold ourselves and fire on crossing it.
  void OnBufferedAmountChange(uint64_t /*previous_amount*/) override {
    if (on_buffered_amount_low_ &&
        channel_->buffered_amount() <= low_threshold_)
      on_buffered_amount_low_(userdata_);
  }

 private:
  webrtc::DataChannelInterface* channel_;  // not owned
  void* userdata_;
  void (*on_message_)(void*, const uint8_t*, size_t, int);
  void (*on_state_change_)(void*, int);
  void (*on_buffered_amount_low_)(void*);
  uint64_t low_threshold_;
};

// A data-channel handle (the `DataChannel` in the Rust API): the channel plus
// its (optional) registered observer.
struct ReactorDataChannel {
  webrtc::scoped_refptr<webrtc::DataChannelInterface> channel;
  std::unique_ptr<ReactorDcObserver> observer;
  uint64_t low_threshold = 0;  // persisted across observer re-registrations
};

// A transceiver handle (the `RtpTransceiver` in the Rust API).
struct ReactorTransceiver {
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> tc;
};

// Owns the three WebRTC threads alongside the factory. The threads must outlive
// the factory, so declare `factory` last: members are destroyed in reverse
// declaration order, releasing the factory before the threads stop.
struct ReactorFactory {
  std::unique_ptr<webrtc::Thread> network_thread;
  std::unique_ptr<webrtc::Thread> worker_thread;
  std::unique_ptr<webrtc::Thread> signaling_thread;
  webrtc::scoped_refptr<FrameAdm> adm;
  webrtc::scoped_refptr<webrtc::PeerConnectionFactoryInterface> factory;
};

// Bridges PeerConnectionObserver callbacks to the C function pointers.
class ReactorPcObserver : public webrtc::PeerConnectionObserver {
 public:
  explicit ReactorPcObserver(const ReactorPcCallbacks& cb) : cb_(cb) {}

  void OnSignalingChange(
      webrtc::PeerConnectionInterface::SignalingState new_state) override {
    if (cb_.on_signaling_change)
      cb_.on_signaling_change(cb_.userdata, static_cast<int>(new_state));
  }
  void OnConnectionChange(
      webrtc::PeerConnectionInterface::PeerConnectionState new_state) override {
    if (cb_.on_connection_change)
      cb_.on_connection_change(cb_.userdata, static_cast<int>(new_state));
  }
  void OnIceGatheringChange(
      webrtc::PeerConnectionInterface::IceGatheringState new_state) override {
    if (cb_.on_ice_gathering_change)
      cb_.on_ice_gathering_change(cb_.userdata, static_cast<int>(new_state));
  }
  void OnIceCandidate(const webrtc::IceCandidate* candidate) override {
    if (!cb_.on_ice_candidate || !candidate) return;
    std::string sdp;
    candidate->ToString(&sdp);
    cb_.on_ice_candidate(cb_.userdata, candidate->sdp_mid().c_str(),
                         candidate->sdp_mline_index(), sdp.c_str());
  }
  void OnDataChannel(
      webrtc::scoped_refptr<webrtc::DataChannelInterface> dc) override {
    if (!cb_.on_data_channel) return;
    cb_.on_data_channel(cb_.userdata,
                        new ReactorDataChannel{std::move(dc), nullptr});
  }
  // Forward both the legacy and the spec-compliant negotiation signals.
  void OnRenegotiationNeeded() override { fire_renegotiation(); }
  void OnNegotiationNeededEvent(uint32_t /*event_id*/) override {
    fire_renegotiation();
  }
  void OnTrack(
      webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> transceiver)
      override {
    if (!cb_.on_track || !transceiver) return;
    webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver =
        transceiver->receiver();
    if (!receiver) return;
    webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track =
        receiver->track();
    if (!track) return;
    auto* handle = new ReactorMediaStreamTrack();
    handle->track = std::move(track);
    cb_.on_track(cb_.userdata, handle);
  }

 private:
  void fire_renegotiation() {
    if (cb_.on_renegotiation_needed) cb_.on_renegotiation_needed(cb_.userdata);
  }
  ReactorPcCallbacks cb_;
};

// Holds the observer alongside the peer connection. The PC holds a raw pointer
// to the observer, so the observer must outlive it: declare `pc` last so it is
// released first.
struct ReactorPeerConnection {
  std::unique_ptr<ReactorPcObserver> observer;
  webrtc::scoped_refptr<webrtc::PeerConnectionInterface> pc;
};

// Forwards CreateOffer/CreateAnswer results to C callbacks.
class CreateSdpObserver : public webrtc::CreateSessionDescriptionObserver {
 public:
  CreateSdpObserver(void* userdata,
                    void (*on_success)(void*, const char*, const char*),
                    void (*on_error)(void*, const char*))
      : userdata_(userdata), on_success_(on_success), on_error_(on_error) {}

  void OnSuccess(webrtc::SessionDescriptionInterface* desc) override {
    std::string sdp;
    desc->ToString(&sdp);
    if (on_success_) on_success_(userdata_, desc->type().c_str(), sdp.c_str());
    // OnSuccess transfers ownership of `desc` to us; we don't keep it here.
    delete desc;
  }
  void OnFailure(webrtc::RTCError error) override {
    if (on_error_) on_error_(userdata_, error.message());
  }

 private:
  void* userdata_;
  void (*on_success_)(void*, const char*, const char*);
  void (*on_error_)(void*, const char*);
};

// Forwards SetLocalDescription completion to a C callback (null error = ok).
class SetLocalDescObserver
    : public webrtc::SetLocalDescriptionObserverInterface {
 public:
  SetLocalDescObserver(void* userdata, void (*on_complete)(void*, const char*))
      : userdata_(userdata), on_complete_(on_complete) {}
  void OnSetLocalDescriptionComplete(webrtc::RTCError error) override {
    if (on_complete_)
      on_complete_(userdata_, error.ok() ? nullptr : error.message());
  }

 private:
  void* userdata_;
  void (*on_complete_)(void*, const char*);
};

// Forwards SetRemoteDescription completion to a C callback (null error = ok).
class SetRemoteDescObserver
    : public webrtc::SetRemoteDescriptionObserverInterface {
 public:
  SetRemoteDescObserver(void* userdata, void (*on_complete)(void*, const char*))
      : userdata_(userdata), on_complete_(on_complete) {}
  void OnSetRemoteDescriptionComplete(webrtc::RTCError error) override {
    if (on_complete_)
      on_complete_(userdata_, error.ok() ? nullptr : error.message());
  }

 private:
  void* userdata_;
  void (*on_complete_)(void*, const char*);
};

// Copy a borrowed C string, mapping null to empty.
std::string str_or_empty(const char* s) { return s ? std::string(s) : std::string(); }

// Apply the caller's configuration to a libwebrtc RTCConfiguration. A null
// `in` keeps the libwebrtc defaults. Credentials travel per server entry, so a
// turn:/turns: URL reaches ice_server_parsing.cc with its username and
// password attached.
void apply_rtc_config(const ReactorRtcConfig* in,
                      webrtc::PeerConnectionInterface::RTCConfiguration& cfg) {
  if (!in) return;

  for (size_t i = 0; i < in->servers_len; ++i) {
    const ReactorIceServer& src = in->servers[i];
    webrtc::PeerConnectionInterface::IceServer srv;
    for (size_t u = 0; u < src.urls_len; ++u) {
      if (src.urls && src.urls[u]) srv.urls.push_back(src.urls[u]);
    }
    if (srv.urls.empty()) continue;
    srv.username = str_or_empty(src.username);
    srv.password = str_or_empty(src.password);
    cfg.servers.push_back(std::move(srv));
  }

  switch (in->ice_transport_type) {
    case 1:  cfg.type = webrtc::PeerConnectionInterface::kRelay;  break;
    case 2:  cfg.type = webrtc::PeerConnectionInterface::kNoHost; break;
    case 3:  cfg.type = webrtc::PeerConnectionInterface::kNone;   break;
    default: cfg.type = webrtc::PeerConnectionInterface::kAll;    break;
  }

  cfg.continual_gathering_policy =
      in->continual_gathering_policy == 1
          ? webrtc::PeerConnectionInterface::GATHER_CONTINUALLY
          : webrtc::PeerConnectionInterface::GATHER_ONCE;

  if (in->min_port > 0 && in->max_port > 0) {
    cfg.set_min_port(in->min_port);
    cfg.set_max_port(in->max_port);
  }

  switch (in->bundle_policy) {
    case 1: cfg.bundle_policy =
                webrtc::PeerConnectionInterface::kBundlePolicyMaxBundle; break;
    case 2: cfg.bundle_policy =
                webrtc::PeerConnectionInterface::kBundlePolicyMaxCompat; break;
    default: break;  // 0 = balanced (libwebrtc default)
  }

  if (in->ice_connection_receiving_timeout_ms > 0)
    cfg.ice_connection_receiving_timeout =
        in->ice_connection_receiving_timeout_ms;

  if (in->ice_check_interval_strong_connectivity_ms > 0)
    cfg.ice_check_interval_strong_connectivity =
        in->ice_check_interval_strong_connectivity_ms;

  switch (in->tcp_candidate_policy) {
    case 1: cfg.tcp_candidate_policy =
                webrtc::PeerConnectionInterface::kTcpCandidatePolicyEnabled;
            break;
    default: break;  // 0 = disabled (libwebrtc default)
  }
}

// Write a NUL-terminated copy of `msg` into `out`, truncated to `cap` bytes.
// Does nothing when `out` is null or `cap` is not positive. Templated on the
// message type: RTCError::message() returns const char* on some milestones and
// a string_view on others, and both convert to std::string.
template <typename S>
static void write_error(char* out, int cap, const S& msg) {
  if (!out || cap <= 0) return;
  const std::string s(msg);
  const size_t n = std::min(s.size(), static_cast<size_t>(cap) - 1);
  std::memcpy(out, s.data(), n);
  out[n] = '\0';
}

// Safely dereference any optional-like field (absl::optional<T>, std::optional<T>,
// or any bool-convertible + dereferenceable type). Returns a zero-constructed T
// when the field has no value. Avoids naming the concrete optional type, which
// changed from RTCStatsMember<T> to absl::optional<T> in M7907.
template <typename M>
static auto stat_val(const M& m) -> std::decay_t<decltype(*m)> {
  using T = std::decay_t<decltype(*m)>;
  return m ? static_cast<T>(*m) : T{};
}

// Parse the string ICE-pair state to the integer encoding used in
// ReactorStatEntry::pair_state.
template <typename S>
static int parse_pair_state(const S& m) {
  if (!m) return 0;
  const std::string& s = *m;
  if (s == "in-progress") return 1;
  if (s == "failed")      return 2;
  if (s == "succeeded")   return 3;
  if (s == "cancelled")   return 4;
  return 0;  // "waiting" or unknown
}

// Collects a WebRTC stats report and serialises the RTCInboundRtpStreamStats,
// RTCOutboundRtpStreamStats, and RTCIceCandidatePairStats entries into the
// flat C ABI array expected by the safe crate.
class StatsCallback : public webrtc::RTCStatsCollectorCallback {
 public:
  StatsCallback(void* userdata,
                void (*callback)(void*, const ReactorStatEntry*, int))
      : userdata_(userdata), callback_(callback) {}

  void OnStatsDelivered(
      const webrtc::scoped_refptr<const webrtc::RTCStatsReport>& report)
      override {
    std::vector<ReactorStatEntry> entries;
    if (report) {
      for (const webrtc::RTCStats& stats : *report) {
        ReactorStatEntry e{};
        if (stats.type() == webrtc::RTCInboundRtpStreamStats::kType) {
          const auto& s = stats.cast_to<webrtc::RTCInboundRtpStreamStats>();
          e.kind              = 0;
          e.ssrc              = stat_val(s.ssrc);
          e.packets_received  = stat_val(s.packets_received);
          e.bytes_received    = stat_val(s.bytes_received);
          e.jitter            = stat_val(s.jitter);
          e.packets_lost      = stat_val(s.packets_lost);
          e.nack_count        = stat_val(s.nack_count);
          e.total_decode_time = stat_val(s.total_decode_time);
          entries.push_back(e);
        } else if (stats.type() == webrtc::RTCOutboundRtpStreamStats::kType) {
          const auto& s = stats.cast_to<webrtc::RTCOutboundRtpStreamStats>();
          e.kind                       = 1;
          e.ssrc                       = stat_val(s.ssrc);
          e.packets_sent               = stat_val(s.packets_sent);
          e.bytes_sent                 = stat_val(s.bytes_sent);
          e.target_bitrate             = stat_val(s.target_bitrate);
          // round_trip_time was removed from RTCOutboundRtpStreamStats in M7907;
          // RTT for the send path is now in RTCRemoteInboundRtpStreamStats.
          e.retransmitted_packets_sent = stat_val(s.retransmitted_packets_sent);
          entries.push_back(e);
        } else if (stats.type() == webrtc::RTCIceCandidatePairStats::kType) {
          const auto& s = stats.cast_to<webrtc::RTCIceCandidatePairStats>();
          e.kind                    = 2;
          e.current_round_trip_time = stat_val(s.current_round_trip_time);
          e.priority                = stat_val(s.priority);
          e.pair_state              = parse_pair_state(s.state);
          entries.push_back(e);
        }
      }
    }
    if (callback_)
      callback_(userdata_, entries.data(), static_cast<int>(entries.size()));

    // Matches the extra AddRef taken in reactor_webrtc_peer_connection_get_stats
    // before this object was handed to GetStats(). GetStats() is proxied onto
    // the signaling thread, and nothing in the public API guarantees the real
    // PeerConnection::GetStats body (which takes its own ref) has run by the
    // time GetStats() returns to the caller. Without this extra ref, the
    // caller's local scoped_refptr can drop the last reference and delete
    // `this` before the queued call ever reaches the signaling thread — a
    // use-after-free that only shows up as a rare, timing-dependent segfault.
    Release();
  }

 private:
  void* userdata_;
  void (*callback_)(void*, const ReactorStatEntry*, int);
};

}  // namespace

// APM flags bitmask (OR together to enable processing stages):
//   0x01  echo_canceller (AEC3)
//   0x02  noise_suppression (kHigh when enabled)
//   0x04  gain_controller1 (AGC)
//   0x08  high_pass_filter
//
// Declared outside extern "C": returns a C++ type (scoped_refptr), which MSVC
// rejects inside extern "C" blocks even for static helper functions.
static webrtc::scoped_refptr<webrtc::AudioProcessing> build_apm(int apm_flags) {
  if (apm_flags == 0) return nullptr;  // no processing → true passthrough
  webrtc::AudioProcessing::Config cfg;
  cfg.echo_canceller.enabled    = (apm_flags & 0x01) != 0;
  cfg.noise_suppression.enabled = (apm_flags & 0x02) != 0;
  if (cfg.noise_suppression.enabled)
    cfg.noise_suppression.level =
        webrtc::AudioProcessing::Config::NoiseSuppression::kHigh;
  cfg.gain_controller1.enabled = (apm_flags & 0x04) != 0;
  cfg.high_pass_filter.enabled = (apm_flags & 0x08) != 0;
  return webrtc::BuiltinAudioProcessingBuilder(cfg)
      .Build(webrtc::CreateEnvironment());
}

extern "C" {

// ABI version of this native build. The safe crate asserts compatibility.
unsigned int reactor_webrtc_abi_version() { return 2; }

// Link/run self-test: build the builtin audio + video encoder factories and
// enumerate the codecs they support. Writes a comma-separated, NUL-terminated
// list of codec names into `out` (truncated to `cap`) and returns the total
// count. A non-zero return means real libwebrtc code linked and executed.
int reactor_webrtc_selftest(char* out, int cap) {
  std::string names;
  int count = 0;

  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> audio =
      webrtc::CreateBuiltinAudioEncoderFactory();
  for (const webrtc::AudioCodecSpec& spec : audio->GetSupportedEncoders()) {
    if (!names.empty()) names += ",";
    names += spec.format.name;
    ++count;
  }

  std::unique_ptr<webrtc::VideoEncoderFactory> video =
      webrtc::CreateBuiltinVideoEncoderFactory();
  for (const webrtc::SdpVideoFormat& fmt : video->GetSupportedFormats()) {
    if (!names.empty()) names += ",";
    names += fmt.name;
    ++count;
  }

  if (out && cap > 0) {
    std::strncpy(out, names.c_str(), static_cast<size_t>(cap) - 1);
    out[cap - 1] = '\0';
  }
  return count;
}

// Create a PeerConnectionFactory: start the network/worker/signaling threads
// and wire the builtin audio+video codec factories.
//
// `use_platform_adm`: 0 → our synthetic FrameAdm (push PCM, no hardware);
// nonzero → pass a null ADM so the media engine creates the platform default
// ADM (real mic/speaker, e.g. CoreAudio on macOS).
// `apm_flags`: bitmask of REACTOR_APM_* flags (0 = all processing disabled).
// Returns an opaque ReactorFactory* or nullptr.
void* reactor_webrtc_factory_create_with_adm_apm(int use_platform_adm,
                                                  int apm_flags) {
  auto f = std::make_unique<ReactorFactory>();

  f->network_thread = webrtc::Thread::CreateWithSocketServer();
  f->worker_thread = webrtc::Thread::Create();
  f->signaling_thread = webrtc::Thread::Create();
  if (!f->network_thread->Start() || !f->worker_thread->Start() ||
      !f->signaling_thread->Start()) {
    return nullptr;
  }

  webrtc::scoped_refptr<webrtc::AudioDeviceModule> adm;
  if (!use_platform_adm) {
    f->adm = webrtc::make_ref_counted<FrameAdm>();
    adm = f->adm;
  }

  auto apm = build_apm(apm_flags);

  f->factory = webrtc::CreatePeerConnectionFactory(
      f->network_thread.get(), f->worker_thread.get(),
      f->signaling_thread.get(), adm,
      webrtc::CreateBuiltinAudioEncoderFactory(),
      webrtc::CreateBuiltinAudioDecoderFactory(),
      webrtc::CreateBuiltinVideoEncoderFactory(),
      webrtc::CreateBuiltinVideoDecoderFactory(),
      /*audio_mixer=*/nullptr, /*audio_processing=*/apm);
  if (!f->factory) {
    return nullptr;
  }
  return f.release();
}

void* reactor_webrtc_factory_create_with_adm(int use_platform_adm) {
  return reactor_webrtc_factory_create_with_adm_apm(use_platform_adm, 0);
}

// Create a factory with the synthetic (push-able) ADM and no APM processing.
void* reactor_webrtc_factory_create() {
  return reactor_webrtc_factory_create_with_adm_apm(0, 0);
}

// Enable/disable the synthetic ADM's playout pump (no-op for the platform ADM).
void reactor_webrtc_factory_set_adm_playout_enabled(void* factory, int enabled) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (rf && rf->adm) rf->adm->SetPlayoutEnabled(enabled != 0);
}

// Destroy a factory created by reactor_webrtc_factory_create (releases the
// factory, then stops + joins the threads).
void reactor_webrtc_factory_destroy(void* factory) {
  delete reinterpret_cast<ReactorFactory*>(factory);
}

// Create a PeerConnection on `factory`. `config` may be null (libwebrtc
// defaults). `callbacks` may be null. Returns an opaque ReactorPeerConnection*
// (the `PeerConnection` handle), or nullptr. On failure the reason from
// libwebrtc goes into `err` (NUL-terminated, truncated to `err_cap`).
void* reactor_webrtc_peer_connection_create(void* factory,
                                            const ReactorRtcConfig* config,
                                            const ReactorPcCallbacks* callbacks,
                                            char* err, int err_cap) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) {
    write_error(err, err_cap, "invalid peer connection factory");
    return nullptr;
  }

  auto rpc = std::make_unique<ReactorPeerConnection>();
  ReactorPcCallbacks cb{};
  if (callbacks) cb = *callbacks;
  rpc->observer = std::make_unique<ReactorPcObserver>(cb);

  webrtc::PeerConnectionInterface::RTCConfiguration rtc_config;
  rtc_config.sdp_semantics = webrtc::SdpSemantics::kUnifiedPlan;
  apply_rtc_config(config, rtc_config);

  webrtc::PeerConnectionDependencies deps(rpc->observer.get());
  auto result =
      rf->factory->CreatePeerConnectionOrError(rtc_config, std::move(deps));
  if (!result.ok()) {
    write_error(err, err_cap, result.error().message());
    return nullptr;
  }
  rpc->pc = result.MoveValue();
  return rpc.release();
}

// Close + destroy a PeerConnection.
void reactor_webrtc_peer_connection_destroy(void* pc) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (rpc && rpc->pc) rpc->pc->Close();
  delete rpc;
}

// Set aggregate bitrate limits on the peer connection. Use -1 for any field
// that should keep the libwebrtc default.
//
//   min_bps   — floor handed to the congestion controller; it will not drop
//               below this even when the network estimate is very low.
//   start_bps — initial encoder target; libwebrtc defaults to ~300 kbps,
//               which causes a slow ramp-up; set to your expected steady-state
//               for streaming (e.g. 4 000 000 for 4 Mbps targets).
//   max_bps   — ceiling; the GCC algorithm will not allocate above this.
//
// All values are in bits per second.
// Returns 0 on success, -1 on error (message written to err/err_cap).
int reactor_webrtc_peer_connection_set_bitrate(void* pc,
                                               int min_bps,
                                               int start_bps,
                                               int max_bps,
                                               char* err,
                                               int err_cap) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    write_error(err, err_cap, "no peer connection");
    return -1;
  }
  webrtc::BitrateSettings s;
  if (min_bps   > 0) s.min_bitrate_bps   = min_bps;
  if (start_bps > 0) s.start_bitrate_bps = start_bps;
  if (max_bps   > 0) s.max_bitrate_bps   = max_bps;
  auto result = rpc->pc->SetBitrate(s);
  if (!result.ok()) {
    write_error(err, err_cap, result.message());
    return -1;
  }
  return 0;
}

// Create an offer. The result is delivered asynchronously on the signaling
// thread to exactly one of the callbacks.
void reactor_webrtc_peer_connection_create_offer(
    void* pc, void* userdata,
    void (*on_success)(void* userdata, const char* type, const char* sdp),
    void (*on_error)(void* userdata, const char* message)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_error) on_error(userdata, "no peer connection");
    return;
  }
  auto observer =
      webrtc::make_ref_counted<CreateSdpObserver>(userdata, on_success, on_error);
  webrtc::PeerConnectionInterface::RTCOfferAnswerOptions options;
  rpc->pc->CreateOffer(observer.get(), options);
}

// Create an answer (current signaling state must have a remote offer). Result
// delivered like create_offer.
void reactor_webrtc_peer_connection_create_answer(
    void* pc, void* userdata,
    void (*on_success)(void* userdata, const char* type, const char* sdp),
    void (*on_error)(void* userdata, const char* message)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_error) on_error(userdata, "no peer connection");
    return;
  }
  auto observer =
      webrtc::make_ref_counted<CreateSdpObserver>(userdata, on_success, on_error);
  webrtc::PeerConnectionInterface::RTCOfferAnswerOptions options;
  rpc->pc->CreateAnswer(observer.get(), options);
}

// Parse (type, sdp) and apply it as the local description. `on_complete` fires
// once with a null error on success.
void reactor_webrtc_peer_connection_set_local_description(
    void* pc, const char* type, const char* sdp, void* userdata,
    void (*on_complete)(void* userdata, const char* error)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_complete) on_complete(userdata, "no peer connection");
    return;
  }
  const std::optional<webrtc::SdpType> t =
      webrtc::SdpTypeFromString(type ? type : "");
  if (!t) {
    if (on_complete) on_complete(userdata, "invalid sdp type");
    return;
  }
  std::unique_ptr<webrtc::SessionDescriptionInterface> desc =
      webrtc::CreateSessionDescription(*t, sdp ? sdp : "");
  if (!desc) {
    if (on_complete) on_complete(userdata, "failed to parse sdp");
    return;
  }
  rpc->pc->SetLocalDescription(
      std::move(desc),
      webrtc::make_ref_counted<SetLocalDescObserver>(userdata, on_complete));
}

// Parse (type, sdp) and apply it as the remote description.
void reactor_webrtc_peer_connection_set_remote_description(
    void* pc, const char* type, const char* sdp, void* userdata,
    void (*on_complete)(void* userdata, const char* error)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_complete) on_complete(userdata, "no peer connection");
    return;
  }
  const std::optional<webrtc::SdpType> t =
      webrtc::SdpTypeFromString(type ? type : "");
  if (!t) {
    if (on_complete) on_complete(userdata, "invalid sdp type");
    return;
  }
  std::unique_ptr<webrtc::SessionDescriptionInterface> desc =
      webrtc::CreateSessionDescription(*t, sdp ? sdp : "");
  if (!desc) {
    if (on_complete) on_complete(userdata, "failed to parse sdp");
    return;
  }
  rpc->pc->SetRemoteDescription(
      std::move(desc),
      webrtc::make_ref_counted<SetRemoteDescObserver>(userdata, on_complete));
}

// Add a remote ICE candidate (from the peer's OnIceCandidate). `on_complete`
// fires once with a null error on success.
void reactor_webrtc_peer_connection_add_ice_candidate(
    void* pc, const char* sdp_mid, int sdp_mline_index, const char* candidate,
    void* userdata, void (*on_complete)(void* userdata, const char* error)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (on_complete) on_complete(userdata, "no peer connection");
    return;
  }
  webrtc::SdpParseError err;
  std::unique_ptr<webrtc::IceCandidate> cand(webrtc::CreateIceCandidate(
      sdp_mid ? sdp_mid : "", sdp_mline_index,
      std::string(candidate ? candidate : ""), &err));
  if (!cand) {
    if (on_complete) on_complete(userdata, err.description.c_str());
    return;
  }
  rpc->pc->AddIceCandidate(
      std::move(cand), [userdata, on_complete](webrtc::RTCError error) {
        if (on_complete)
          on_complete(userdata, error.ok() ? nullptr : error.message());
      });
}

// Create a (negotiated-by-SDP) data channel. Returns an opaque DataChannel
// handle (a heap scoped_refptr) or nullptr.
void* reactor_webrtc_peer_connection_create_data_channel(void* pc,
                                                         const char* label) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) return nullptr;
  webrtc::DataChannelInit init;
  auto result = rpc->pc->CreateDataChannelOrError(label ? label : "", &init);
  if (!result.ok()) return nullptr;
  return new ReactorDataChannel{result.MoveValue(), nullptr};
}

// Send bytes over a data channel. Returns 1 on success, 0 on failure.
int reactor_webrtc_data_channel_send(void* data_channel, const uint8_t* data,
                                     size_t len, int binary) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel) return 0;
  webrtc::CopyOnWriteBuffer buffer(data, len);
  return h->channel->Send(webrtc::DataBuffer(buffer, binary != 0)) ? 1 : 0;
}

// Register callbacks for a data channel.
// `on_message(userdata, data, len, binary)` fires per message.
// `on_state_change(userdata, state)` fires on all state transitions;
//   state: 0=Connecting 1=Open 2=Closing 3=Closed.
// `on_buffered_amount_low(userdata)` fires when buffered_amount drops at or
//   below the threshold set by reactor_webrtc_data_channel_set_low_threshold.
// Any pointer may be null. Replaces any previously registered observer.
void reactor_webrtc_data_channel_register_observer(
    void* data_channel, void* userdata,
    void (*on_message)(void*, const uint8_t*, size_t, int),
    void (*on_state_change)(void*, int),
    void (*on_buffered_amount_low)(void*)) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel) return;
  if (h->observer) h->channel->UnregisterObserver();
  h->observer = std::make_unique<ReactorDcObserver>(
      h->channel.get(), userdata, on_message, on_state_change,
      on_buffered_amount_low, h->low_threshold);
  h->channel->RegisterObserver(h->observer.get());
}

// Returns the number of bytes currently queued for sending.
uint64_t reactor_webrtc_data_channel_buffered_amount(void* data_channel) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel) return 0;
  return h->channel->buffered_amount();
}

// Copies the channel label into `out` (NUL-terminated, capped at `cap`).
// Returns the label length (may exceed cap if truncated), or -1 on error.
int reactor_webrtc_data_channel_label(void* data_channel, char* out, int cap) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel || !out || cap <= 0) return -1;
  const std::string& label = h->channel->label();
  int n = static_cast<int>(label.size());
  int copy = std::min(n, cap - 1);
  std::memcpy(out, label.data(), copy);
  out[copy] = '\0';
  return n;
}

// Returns the current channel state: 0=Connecting 1=Open 2=Closing 3=Closed.
int reactor_webrtc_data_channel_state(void* data_channel) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h || !h->channel) return 3;
  switch (h->channel->state()) {
    case webrtc::DataChannelInterface::kConnecting: return 0;
    case webrtc::DataChannelInterface::kOpen:       return 1;
    case webrtc::DataChannelInterface::kClosing:    return 2;
    default:                                        return 3;
  }
}

// Sets the buffered-amount-low threshold. on_buffered_amount_low fires when
// the buffered amount drops to this value or below after a send.
// M7907 removed SetBufferedAmountLowThreshold from the public API; the
// threshold is tracked in ReactorDataChannel/ReactorDcObserver instead.
void reactor_webrtc_data_channel_set_low_threshold(void* data_channel,
                                                    uint64_t threshold) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (!h) return;
  h->low_threshold = threshold;
  if (h->observer) h->observer->SetLowThreshold(threshold);
}

// Destroy a DataChannel handle (unregisters its observer, releases the channel).
void reactor_webrtc_data_channel_destroy(void* data_channel) {
  auto* h = reinterpret_cast<ReactorDataChannel*>(data_channel);
  if (h && h->channel && h->observer) h->channel->UnregisterObserver();
  delete h;
}

// ── Video tracks ──────────────────────────────────────────────────────────────

// Create a local video track backed by a push-able source. Returns an opaque
// MediaStreamTrack handle (free with reactor_webrtc_media_stream_track_destroy)
// or nullptr.
void* reactor_webrtc_video_track_create(void* factory, const char* id) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) return nullptr;
  auto handle = std::make_unique<ReactorMediaStreamTrack>();
  handle->source = webrtc::make_ref_counted<FrameSource>();
  handle->track = rf->factory->CreateVideoTrack(handle->source, id ? id : "");
  if (!handle->track) return nullptr;
  return handle.release();
}

// Push a BGRA frame (width*height*4 bytes) into a local video track's source.
// Converted to I420 and timestamped here.
// Returns the current monotonic clock in microseconds — the same epoch used
// by VideoFrame::set_timestamp_us and EncodedImage::CaptureTime().
int64_t reactor_webrtc_time_micros() { return webrtc::TimeMicros(); }

// Like reactor_webrtc_video_track_push_frame but uses a caller-supplied
// capture timestamp so Rust can key per-frame metadata by that timestamp.
void reactor_webrtc_video_track_push_frame_ts(void* track, const uint8_t* bgra,
                                              int width, int height,
                                              int64_t capture_time_us) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->source || !bgra || width <= 0 || height <= 0) return;
  webrtc::scoped_refptr<webrtc::I420Buffer> buffer =
      webrtc::I420Buffer::Create(width, height);
  // libyuv "ARGB" is B,G,R,A in memory == our BGRA byte order.
  libyuv::ARGBToI420(bgra, width * 4, buffer->MutableDataY(), buffer->StrideY(),
                     buffer->MutableDataU(), buffer->StrideU(),
                     buffer->MutableDataV(), buffer->StrideV(), width, height);
  webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
                                 .set_video_frame_buffer(buffer)
                                 .set_timestamp_us(capture_time_us)
                                 .build();
  h->source->PushFrame(frame);
}

void reactor_webrtc_video_track_push_frame(void* track, const uint8_t* bgra,
                                           int width, int height) {
  reactor_webrtc_video_track_push_frame_ts(track, bgra, width, height,
                                           webrtc::TimeMicros());
}

// Add a (local) audio or video track to the peer connection, creating a
// sendrecv transceiver. Returns 1 on success, 0 on failure.
int reactor_webrtc_peer_connection_add_track(void* pc, void* track) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!rpc || !rpc->pc || !h || !h->track) return 0;
  return rpc->pc->AddTrack(h->track, {kReactorStreamId}).ok() ? 1 : 0;
}

// Create a local audio track. Its samples come from the factory's ADM (push
// PCM with reactor_webrtc_factory_push_audio_frame). Returns an owned
// MediaStreamTrack handle or nullptr.
void* reactor_webrtc_audio_track_create(void* factory, const char* id) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) return nullptr;
  webrtc::scoped_refptr<webrtc::AudioSourceInterface> source =
      rf->factory->CreateAudioSource(webrtc::AudioOptions());
  auto handle = std::make_unique<ReactorMediaStreamTrack>();
  handle->track = rf->factory->CreateAudioTrack(id ? id : "", source.get());
  if (!handle->track) return nullptr;
  return handle.release();
}

// Create a local audio track backed by a per-track LocalAudioSource instead of
// the factory-level ADM. Each call returns an independent source, so different
// audio can be pushed to different peer connections. Feed via
// reactor_webrtc_audio_track_push_pcm.
void* reactor_webrtc_audio_track_create_with_local_source(void* factory,
                                                          const char* id) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->factory) return nullptr;
  auto source = LocalAudioSource::Create();
  auto handle = std::make_unique<ReactorMediaStreamTrack>();
  handle->audio_source = source;
  handle->track = rf->factory->CreateAudioTrack(id ? id : "", source.get());
  if (!handle->track) return nullptr;
  return handle.release();
}

// Push interleaved int16 PCM directly to a local audio track that was created
// with reactor_webrtc_audio_track_create_with_local_source. No-op on tracks
// backed by the factory ADM.
void reactor_webrtc_audio_track_push_pcm(void* track, const int16_t* pcm,
                                         int samples_per_channel,
                                         int sample_rate, int channels) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->audio_source || !pcm || samples_per_channel <= 0 || channels <= 0)
    return;
  h->audio_source->PushPcm(pcm, samples_per_channel, sample_rate, channels);
}

// Deliver interleaved int16 PCM to the factory's ADM (shared by all local audio
// tracks). `samples_per_channel` is the frame count (e.g. 480 for 10ms@48kHz).
void reactor_webrtc_factory_push_audio_frame(void* factory, const int16_t* pcm,
                                             int samples_per_channel,
                                             int sample_rate, int channels) {
  auto* rf = reinterpret_cast<ReactorFactory*>(factory);
  if (!rf || !rf->adm || samples_per_channel <= 0 || channels <= 0) return;
  rf->adm->PushPcm(pcm, static_cast<size_t>(samples_per_channel),
                   static_cast<uint32_t>(sample_rate),
                   static_cast<size_t>(channels));
}

// Attach a frame sink to a (received) audio track. `on_audio(userdata,
// sample_rate, channels, frames)` fires per 10ms block until destroyed.
void reactor_webrtc_audio_track_add_sink(
    void* track, void* userdata,
    void (*on_audio)(void*, const int16_t*, int, int, int)) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track) {
    if (audio_debug()) fprintf(stderr, "[reactor-webrtc] add_audio_sink: null track\n");
    return;
  }
  if (h->track->kind() != "audio") {
    if (audio_debug())
      fprintf(stderr, "[reactor-webrtc] add_audio_sink: track kind=%s (not audio) — skipped\n",
              h->track->kind().c_str());
    return;
  }
  h->audio_sink = std::make_unique<AudioFrameSink>(userdata, on_audio);
  auto* at = static_cast<webrtc::AudioTrackInterface*>(h->track.get());
  at->AddSink(h->audio_sink.get());
  if (audio_debug())
    fprintf(stderr,
            "[reactor-webrtc] add_audio_sink: AddSink OK (track=%p enabled=%d state=%d)\n",
            static_cast<void*>(at), at->enabled(), static_cast<int>(at->state()));
}

// Attach a frame sink to a (video) track. `on_frame(userdata, width, height)`
// fires per decoded frame. The sink lives until the track handle is destroyed.
void reactor_webrtc_video_track_add_sink(
    void* track, void* userdata,
    void (*on_frame)(void*, const uint8_t*, int, int)) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track || h->track->kind() != "video") return;
  h->sink = std::make_unique<FrameSink>(userdata, on_frame);
  static_cast<webrtc::VideoTrackInterface*>(h->track.get())
      ->AddOrUpdateSink(h->sink.get(), webrtc::VideoSinkWants());
}

// Kind of a track handle: 0 = audio, 1 = video, -1 = unknown.
int reactor_webrtc_media_stream_track_kind(void* track) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h || !h->track) return -1;
  const std::string kind = h->track->kind();
  if (kind == "audio") return 0;
  if (kind == "video") return 1;
  return -1;
}

// ── Transceivers ──────────────────────────────────────────────────────────────

// Add a transceiver of `media_kind` (0=audio, 1=video) with `direction`
// (0=sendrecv, 1=sendonly, 2=recvonly, 3=inactive). Returns an opaque
// RtpTransceiver handle (free with reactor_webrtc_rtp_transceiver_destroy).
void* reactor_webrtc_peer_connection_add_transceiver(void* pc, int media_kind,
                                                     int direction) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) return nullptr;
  webrtc::MediaType mt =
      media_kind == 0 ? webrtc::MediaType::AUDIO : webrtc::MediaType::VIDEO;
  webrtc::RtpTransceiverInit init;
  init.direction = static_cast<webrtc::RtpTransceiverDirection>(direction);
  auto result = rpc->pc->AddTransceiver(mt, init);
  if (!result.ok()) return nullptr;
  return new ReactorTransceiver{result.MoveValue()};
}

// Number of transceivers on the peer connection (post-negotiation this
// includes ones auto-created from the remote description).
int reactor_webrtc_peer_connection_transceiver_count(void* pc) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) return 0;
  return static_cast<int>(rpc->pc->GetTransceivers().size());
}

// Return a new owned handle to the transceiver at `index`
// (free with reactor_webrtc_rtp_transceiver_destroy), or null if out of range.
void* reactor_webrtc_peer_connection_get_transceiver(void* pc, int index) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc || index < 0) return nullptr;
  auto tcs = rpc->pc->GetTransceivers();
  if (static_cast<size_t>(index) >= tcs.size()) return nullptr;
  return new ReactorTransceiver{tcs[static_cast<size_t>(index)]};
}

// Media kind of a transceiver: 0 = audio, 1 = video, -1 = unknown.
int reactor_webrtc_rtp_transceiver_media_kind(void* transceiver) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return -1;
  switch (h->tc->media_type()) {
    case webrtc::MediaType::AUDIO:
      return 0;
    case webrtc::MediaType::VIDEO:
      return 1;
    default:
      return -1;
  }
}

// Write the transceiver's mid into `out` (NUL-terminated, capped at `cap`).
// Returns the mid length, or -1 if there is no mid yet (before SLD).
int reactor_webrtc_rtp_transceiver_mid(void* transceiver, char* out, int cap) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return -1;
  const std::optional<std::string> mid = h->tc->mid();
  if (!mid) return -1;
  if (out && cap > 0) {
    std::strncpy(out, mid->c_str(), static_cast<size_t>(cap) - 1);
    out[cap - 1] = '\0';
  }
  return static_cast<int>(mid->size());
}

// Attach (or clear, with null) a local track on the transceiver's sender.
// Returns 1 on success, 0 on failure.
//
// An attached track joins kReactorStreamId, matching what AddTrack publishes,
// so a sender wired up through a transceiver signals a real msid instead of
// "a=msid:-". The next create_offer/create_answer carries it. SetStreams shares
// SetTrack's signaling-thread contract, so it adds no threading obligation of
// its own.
int reactor_webrtc_rtp_transceiver_set_track(void* transceiver, void* track) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return 0;
  auto* t = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  webrtc::MediaStreamTrackInterface* raw = (t && t->track) ? t->track.get() : nullptr;
  auto sender = h->tc->sender();
  if (!sender || !sender->SetTrack(raw)) return 0;
  if (raw) {
    // Only when it would change something: SetStreams signals
    // negotiation-needed, and a replaceTrack on an established connection
    // already carries the id its predecessor was published under.
    const std::vector<std::string> ids = sender->stream_ids();
    if (ids.size() != 1 || ids[0] != kReactorStreamId) {
      sender->SetStreams({kReactorStreamId});
    }
  }
  return 1;
}

// Set the transceiver's direction. direction: 0=sendrecv, 1=sendonly,
// 2=recvonly, 3=inactive. Returns 1 on success, 0 on failure.
int reactor_webrtc_rtp_transceiver_set_direction(void* transceiver, int direction) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return 0;
  auto dir = static_cast<webrtc::RtpTransceiverDirection>(direction);
  auto err = h->tc->SetDirectionWithError(dir);
  return err.ok() ? 1 : 0;
}

// Identity of the *transceiver* itself, as an opaque value — not an owning
// handle.
//
// Unlike the ReactorTransceiver handle, which is a fresh heap object on every
// transceivers() call, the native RtpTransceiverInterface behind it is stable for
// the life of the transceiver. That makes this usable as a key from the moment the
// transceiver exists — before any track is attached, and before the first SDP
// exchange assigns a mid.
uintptr_t reactor_webrtc_rtp_transceiver_id(void* transceiver) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc) return 0;
  return reinterpret_cast<uintptr_t>(h->tc.get());
}

// Identity of the track currently attached to this transceiver's sender, as an
// opaque value — NOT an owning handle.
//
// The pointer is only ever compared, never dereferenced or released, and no
// reference is taken: the caller uses it to recognise which of its own tracks
// this transceiver is sending. Returns 0 when the sender has no track.
//
// A `void*` rather than a ReactorMediaStreamTrack: the native
// MediaStreamTrackInterface is what two Rust wrappers around the same track have
// in common, which is exactly the identity being asked for. Wrapping it in a new
// handle would produce an object with its own state and defeat the purpose.
uintptr_t reactor_webrtc_rtp_transceiver_sender_track_id(void* transceiver) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc || !h->tc->sender()) return 0;
  auto track = h->tc->sender()->track();
  return reinterpret_cast<uintptr_t>(track.get());
}

// Identity of the track this transceiver's receiver delivers, on the same
// non-owning terms as reactor_webrtc_rtp_transceiver_sender_track_id.
//
// Available once the remote description has been applied — the receiver's track
// is created while applying it, which is the same point on_track fires.
uintptr_t reactor_webrtc_rtp_transceiver_receiver_track_id(void* transceiver) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  if (!h || !h->tc || !h->tc->receiver()) return 0;
  auto track = h->tc->receiver()->track();
  return reinterpret_cast<uintptr_t>(track.get());
}

// Destroy a transceiver handle (releases our reference).
void reactor_webrtc_rtp_transceiver_destroy(void* transceiver) {
  delete reinterpret_cast<ReactorTransceiver*>(transceiver);
}

// ── Encoded-frame transform (codec bypass / forward) ─────────────────────────
//
// WebRTC's Insertable Streams / Encoded Transform: a FrameTransformerInterface
// attached to a sender (SetFrameTransformer via
// SetEncoderToPacketizerFrameTransformer) sees each *encoded* frame after the
// encoder and before packetization; on a receiver
// (SetDepacketizerToDecoderFrameTransformer) it sees each encoded frame after
// depacketization and before the decoder. The bindings can read the encoded
// payload (to forward it elsewhere), optionally replace it, and choose whether
// to emit it downstream — dropping it on the receive side bypasses the decoder.

extern "C" {
// Encoded frame handed to the callback. `data`/`mime_type` are valid only for
// the duration of the call. `frame` is an opaque handle for set_data.
struct ReactorEncodedFrame {
  int direction;           // 0 = send (egress), 1 = receive (ingress)
  int is_audio;            // 1 = audio, 0 = video
  int is_key_frame;        // video only (0 for audio)
  uint8_t payload_type;
  uint32_t ssrc;
  uint32_t timestamp;      // RTP timestamp
  int64_t capture_time_ms; // capture timestamp in ms (same epoch as TimeMicros); 0 if unavailable
  const uint8_t* data;     // encoded payload
  size_t data_len;
  const char* mime_type;   // e.g. "video/VP8", "audio/opus"
  void* frame;             // opaque -> reactor_webrtc_encoded_frame_set_data
};
// Return 0 to emit the frame downstream (after any set_data), non-zero to drop
// it (receive side: bypasses the decoder; send side: nothing is sent).
typedef int (*reactor_webrtc_encoded_frame_cb)(void* userdata,
                                               const ReactorEncodedFrame* frame);
// Called once when the transformer is finally destroyed (all refs dropped) so
// the binding can free `userdata`. May be null.
typedef void (*reactor_webrtc_userdata_free)(void* userdata);
}

// ── Custom video encoder (encoder bypass) ────────────────────────────────────
//
// Raw I420 frame delivered to Rust for encoding. Planes are valid only for
// the duration of the Encode() call — copy if encoding is asynchronous.
//
// `codec` mirrors webrtc::VideoCodecType: VP8=1, VP9=2, AV1=3, H264=4, H265=5.
// It tells the callback which codec was negotiated for this session so the
// application can produce the right bitstream.
extern "C" {
struct ReactorRawVideoFrame {
  const uint8_t* y;
  int            y_stride;
  const uint8_t* u;
  int            u_stride;
  const uint8_t* v;
  int            v_stride;
  uint32_t       width;
  uint32_t       height;
  uint32_t       rtp_timestamp;
  int            request_key_frame; // 1 = IDR/keyframe requested
  uint32_t       codec;             // VideoCodecType value (VP8=1 … H265=5)
  uint64_t       encoder_id;       // unique per ReactorVideoEncoder instance
};
// Filled by the Rust callback to deliver an encoded frame back.
// Set data=nullptr (or return non-zero) to drop the frame (nothing is sent).
// `free_data` is called after the encoded bytes are copied; allows the caller
// to free the buffer. May be null if the buffer has static/frame lifetime.
struct ReactorEncodedVideoOutput {
  const uint8_t* data;
  size_t         len;
  int            is_key_frame;
  uint32_t       width;         // 0 = inherit from raw frame
  uint32_t       height;        // 0 = inherit from raw frame
  uint32_t       rtp_timestamp; // 0 = inherit from raw frame
  void           (*free_data)(const uint8_t* data, size_t len); // may be null
};
// Return 0 to forward the encoded frame, non-zero to drop it.
typedef int (*reactor_video_encode_cb)(void*                      userdata,
                                       const ReactorRawVideoFrame* raw,
                                       ReactorEncodedVideoOutput*  out);
// Optional: called by the factory before creating each VideoEncoder instance.
// Return non-zero to delegate to the builtin VP8/VP9/AV1 encoder instead of
// the custom one. May be null (always use custom). `encoder_id` matches the
// value stamped on every subsequent ReactorRawVideoFrame from that encoder.
typedef int (*reactor_use_builtin_cb)(void* userdata, uint64_t encoder_id);
}

struct ReactorEncoderState {
  reactor_video_encode_cb       cb;
  void*                          userdata;
  reactor_webrtc_userdata_free   free_ud;
  reactor_use_builtin_cb         use_builtin; // null = always use custom

  ReactorEncoderState(reactor_video_encode_cb c, void* u, reactor_webrtc_userdata_free f,
                      reactor_use_builtin_cb ub = nullptr)
      : cb(c), userdata(u), free_ud(f), use_builtin(ub) {}
  ~ReactorEncoderState() { if (free_ud) free_ud(userdata); }
  // Disable copy+move: free_ud would fire twice (once on the copy, once on
  // the original) which would double-free `userdata`.
  ReactorEncoderState(const ReactorEncoderState&) = delete;
  ReactorEncoderState(ReactorEncoderState&&)      = delete;
};

class ReactorVideoEncoder : public webrtc::VideoEncoder {
  std::shared_ptr<ReactorEncoderState> state_;
  webrtc::EncodedImageCallback*        callback_   = nullptr;
  uint32_t                             width_      = 0;
  uint32_t                             height_     = 0;
  webrtc::VideoCodecType               codec_type_ = webrtc::kVideoCodecGeneric;
  uint64_t                             id_         = 0;

 public:
  explicit ReactorVideoEncoder(std::shared_ptr<ReactorEncoderState> state,
                               uint64_t id = 0)
      : state_(std::move(state)), id_(id) {}

  int InitEncode(const webrtc::VideoCodec* settings,
                 const Settings&) override {
    if (settings) {
      width_      = static_cast<uint32_t>(settings->width);
      height_     = static_cast<uint32_t>(settings->height);
      codec_type_ = settings->codecType;
    }
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t RegisterEncodeCompleteCallback(
      webrtc::EncodedImageCallback* cb) override {
    callback_ = cb;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Release() override {
    callback_ = nullptr;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t Encode(
      const webrtc::VideoFrame&                  frame,
      const std::vector<webrtc::VideoFrameType>* frame_types) override {
    // Convert to I420 (handles any input format including NV12).
    webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 =
        frame.video_frame_buffer()->ToI420();

    bool want_key = false;
    if (frame_types) {
      for (auto ft : *frame_types)
        if (ft == webrtc::VideoFrameType::kVideoFrameKey) want_key = true;
    }

    ReactorRawVideoFrame raw{};
    raw.y                 = i420->DataY();
    raw.y_stride          = i420->StrideY();
    raw.u                 = i420->DataU();
    raw.u_stride          = i420->StrideU();
    raw.v                 = i420->DataV();
    raw.v_stride          = i420->StrideV();
    raw.width             = static_cast<uint32_t>(i420->width());
    raw.height            = static_cast<uint32_t>(i420->height());
    raw.rtp_timestamp     = frame.rtp_timestamp();
    raw.request_key_frame = want_key ? 1 : 0;
    raw.codec             = static_cast<uint32_t>(codec_type_);
    raw.encoder_id        = id_;

    ReactorEncodedVideoOutput out{};
    int drop = state_->cb(state_->userdata, &raw, &out);
    if (drop || !out.data || out.len == 0 || !callback_) {
      return WEBRTC_VIDEO_CODEC_OK;
    }

    webrtc::EncodedImage img;
    img.SetEncodedData(webrtc::EncodedImageBuffer::Create(out.data, out.len));
    img.SetFrameType(out.is_key_frame ? webrtc::VideoFrameType::kVideoFrameKey
                                       : webrtc::VideoFrameType::kVideoFrameDelta);
    img._encodedWidth  = out.width  ? out.width  : raw.width;
    img._encodedHeight = out.height ? out.height : raw.height;
    img.SetRtpTimestamp(out.rtp_timestamp ? out.rtp_timestamp : raw.rtp_timestamp);

    // Build codec-specific metadata that the RTP packetizer needs.
    webrtc::CodecSpecificInfo info;
    info.codecType = codec_type_;
    switch (codec_type_) {
      case webrtc::kVideoCodecH264:
        info.codecSpecific.H264.packetization_mode =
            webrtc::H264PacketizationMode::NonInterleaved;
        info.codecSpecific.H264.temporal_idx  = webrtc::kNoTemporalIdx;
        info.codecSpecific.H264.idr_frame     = (out.is_key_frame != 0);
        info.codecSpecific.H264.base_layer_sync = false;
        break;
      case webrtc::kVideoCodecVP8:
        // keyIdx<0 means "don't include in RTP header"; temporalIdx=0 = base layer.
        info.codecSpecific.VP8.keyIdx      = -1;
        info.codecSpecific.VP8.temporalIdx = webrtc::kNoTemporalIdx;
        info.codecSpecific.VP8.layerSync   = false;
        info.codecSpecific.VP8.nonReference = false;
        break;
      case webrtc::kVideoCodecVP9:
        info.codecSpecific.VP9.inter_pic_predicted   = !out.is_key_frame;
        info.codecSpecific.VP9.first_frame_in_picture = true;
        info.codecSpecific.VP9.num_spatial_layers     = 1;
        info.codecSpecific.VP9.temporal_idx           = webrtc::kNoTemporalIdx;
        info.codecSpecific.VP9.temporal_up_switch     = false;
        info.codecSpecific.VP9.ss_data_available      = false;
        break;
      default:
        // AV1, H265, Generic: codecType is sufficient; no union fields needed.
        break;
    }

    callback_->OnEncodedImage(img, &info);
    // Release the encoded buffer now that OnEncodedImage has copied it.
    if (out.free_data) out.free_data(out.data, out.len);
    return WEBRTC_VIDEO_CODEC_OK;
  }

  void SetRates(const RateControlParameters&) override {}

  EncoderInfo GetEncoderInfo() const override {
    EncoderInfo info;
    info.implementation_name     = "ReactorCustom";
    info.is_hardware_accelerated = true;
    return info;
  }
};

class ReactorVideoEncoderFactory : public webrtc::VideoEncoderFactory {
  std::shared_ptr<ReactorEncoderState>        state_;
  std::atomic<uint64_t>                       next_id_{0};
  std::unique_ptr<webrtc::VideoEncoderFactory> builtin_;

 public:
  explicit ReactorVideoEncoderFactory(std::shared_ptr<ReactorEncoderState> s)
      : state_(std::move(s)),
        builtin_(webrtc::CreateBuiltinVideoEncoderFactory()) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return {
        webrtc::SdpVideoFormat::VP8(),
        webrtc::SdpVideoFormat::VP9Profile0(),
        webrtc::SdpVideoFormat("H264", {{"level-asymmetry-allowed", "1"},
                                         {"packetization-mode", "1"},
                                         {"profile-level-id", "42e01f"}}),
        webrtc::SdpVideoFormat::AV1Profile0(),
        webrtc::SdpVideoFormat::H265(),
    };
  }

  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    uint64_t id = next_id_.fetch_add(1, std::memory_order_relaxed);
    if (state_->use_builtin && state_->use_builtin(state_->userdata, id)) {
      return builtin_->Create(env, format);
    }
    return std::make_unique<ReactorVideoEncoder>(state_, id);
  }
};

// A no-op decoder for codecs not present in the builtin factory (H264, H265 in
// this build which was compiled without WEBRTC_USE_H264). It claims support so
// that SDP negotiation succeeds with peers that only advertise those codecs, but
// discards every received frame. Right for send-only sessions or media servers.
class ReactorNullVideoDecoder : public webrtc::VideoDecoder {
  std::string name_;
 public:
  explicit ReactorNullVideoDecoder(std::string name) : name_(std::move(name)) {}
  bool Configure(const Settings&) override { return true; }
  int32_t Decode(const webrtc::EncodedImage&, int64_t) override { return 0; }
  int32_t RegisterDecodeCompleteCallback(webrtc::DecodedImageCallback*) override {
    return 0;
  }
  int32_t Release() override { return 0; }
  DecoderInfo GetDecoderInfo() const override {
    return {name_, /*is_hardware_accelerated=*/false};
  }
};

// Wraps the builtin video decoder factory and adds null decoders for H264 and
// H265, which this libwebrtc build (compiled without WEBRTC_USE_H264) does not
// include. VP8, VP9, and AV1 are handled by the builtin factory as usual.
class ReactorCustomDecoderFactory : public webrtc::VideoDecoderFactory {
  std::unique_ptr<webrtc::VideoDecoderFactory> builtin_;

  static bool IsBuiltinCodec(const webrtc::SdpVideoFormat& f) {
    return f.name == "VP8" || f.name == "VP9" || f.name == "AV1";
  }

 public:
  ReactorCustomDecoderFactory()
      : builtin_(webrtc::CreateBuiltinVideoDecoderFactory()) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    auto formats = builtin_->GetSupportedFormats();
    formats.push_back(webrtc::SdpVideoFormat(
        "H264", {{"level-asymmetry-allowed", "1"},
                 {"packetization-mode", "1"},
                 {"profile-level-id", "42e01f"}}));
    formats.push_back(webrtc::SdpVideoFormat::H265());
    return formats;
  }

  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (IsBuiltinCodec(format)) return builtin_->Create(env, format);
    return std::make_unique<ReactorNullVideoDecoder>("ReactorNull_" + format.name);
  }
};

// Create a PeerConnectionFactory that routes all video encoding through `cb`.
// `cb` is called synchronously inside VideoEncoder::Encode() with the raw I420
// frame; fill `*out` and return 0 to inject bytes into the RTP stack, or
// return non-zero to drop. `free_ud` is called when all encoder instances are
// gone (follows the same lifetime contract as frame_transformer_create).
void* reactor_webrtc_factory_create_with_custom_video_encoder(
    int use_platform_adm, reactor_video_encode_cb cb, void* userdata,
    reactor_webrtc_userdata_free free_ud, reactor_use_builtin_cb use_builtin,
    int apm_flags) {
  auto state = std::make_shared<ReactorEncoderState>(cb, userdata, free_ud, use_builtin);

  auto f = std::make_unique<ReactorFactory>();
  f->network_thread   = webrtc::Thread::CreateWithSocketServer();
  f->worker_thread    = webrtc::Thread::Create();
  f->signaling_thread = webrtc::Thread::Create();
  if (!f->network_thread->Start() || !f->worker_thread->Start() ||
      !f->signaling_thread->Start()) {
    return nullptr;
  }

  webrtc::scoped_refptr<webrtc::AudioDeviceModule> adm;
  if (!use_platform_adm) {
    f->adm = webrtc::make_ref_counted<FrameAdm>();
    adm    = f->adm;
  }

  auto apm = build_apm(apm_flags);

  f->factory = webrtc::CreatePeerConnectionFactory(
      f->network_thread.get(), f->worker_thread.get(),
      f->signaling_thread.get(), adm,
      webrtc::CreateBuiltinAudioEncoderFactory(),
      webrtc::CreateBuiltinAudioDecoderFactory(),
      std::make_unique<ReactorVideoEncoderFactory>(state),
      std::make_unique<ReactorCustomDecoderFactory>(),
      /*audio_mixer=*/nullptr, /*audio_processing=*/apm);
  if (!f->factory) return nullptr;
  return f.release();
}

// Replace the encoded payload of the frame currently in the callback. Copies.
void reactor_webrtc_encoded_frame_set_data(void* frame, const uint8_t* data,
                                           size_t len) {
  if (!frame || !data) return;
  auto* f = reinterpret_cast<webrtc::TransformableFrameInterface*>(frame);
  f->SetData(std::span<const uint8_t>(data, len));
}

class ReactorFrameTransformer : public webrtc::FrameTransformerInterface {
 public:
  ReactorFrameTransformer(reactor_webrtc_encoded_frame_cb cb, void* userdata,
                          reactor_webrtc_userdata_free free_ud)
      : cb_(cb), userdata_(userdata), free_ud_(free_ud) {}
  // Runs when the last ref drops (senders/receivers hold their own), so the
  // binding's userdata outlives every possible callback.
  ~ReactorFrameTransformer() override {
    if (free_ud_) free_ud_(userdata_);
  }

  // Single callback: used by senders (one transform per sender).
  void RegisterTransformedFrameCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> cb) override {
    std::lock_guard<std::mutex> lock(mu_);
    send_sink_ = std::move(cb);
  }
  void UnregisterTransformedFrameCallback() override {
    std::lock_guard<std::mutex> lock(mu_);
    send_sink_ = nullptr;
  }
  // Per-ssrc callback: used by receivers (one transform can serve many ssrcs).
  void RegisterTransformedFrameSinkCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> cb,
      uint32_t ssrc) override {
    std::lock_guard<std::mutex> lock(mu_);
    recv_sinks_[ssrc] = std::move(cb);
  }
  void UnregisterTransformedFrameSinkCallback(uint32_t ssrc) override {
    std::lock_guard<std::mutex> lock(mu_);
    recv_sinks_.erase(ssrc);
  }

  void Transform(
      std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    if (!frame) return;
    // Pick the sink for this frame: receive frames route by ssrc, send frames
    // use the single sender sink.
    webrtc::scoped_refptr<webrtc::TransformedFrameCallback> sink;
    {
      std::lock_guard<std::mutex> lock(mu_);
      auto it = recv_sinks_.find(frame->GetSsrc());
      if (it != recv_sinks_.end())
        sink = it->second;
      else
        sink = send_sink_;
    }

    int emit = 0;
    if (cb_) {
      // RTTI is on (use_rtti=true), so dynamic_cast distinguishes video/audio
      // and exposes IsKeyFrame.
      int is_audio = 0, is_key = 0;
      if (auto* v = dynamic_cast<webrtc::TransformableVideoFrameInterface*>(
              frame.get())) {
        is_key = v->IsKeyFrame() ? 1 : 0;
      } else if (dynamic_cast<webrtc::TransformableAudioFrameInterface*>(
                     frame.get())) {
        is_audio = 1;
      }
      const std::span<const uint8_t> data = frame->GetData();
      const std::string mime = frame->GetMimeType();
      const int direction =
          frame->GetDirection() ==
                  webrtc::TransformableFrameInterface::Direction::kReceiver
              ? 1
              : 0;
      auto ct = frame->CaptureTime();
      int64_t capture_ms = ct.has_value() ? ct->ms() : 0;
      ReactorEncodedFrame ef{
          direction,
          is_audio,
          is_key,
          frame->GetPayloadType(),
          frame->GetSsrc(),
          frame->GetTimestamp(),
          capture_ms,
          data.data(),
          data.size(),
          mime.c_str(),
          frame.get(),
      };
      emit = cb_(userdata_, &ef);
    }

    if (emit == 0 && sink) sink->OnTransformedFrame(std::move(frame));
    // else: dropped (decoder bypassed on receive; nothing sent on egress).
  }

 private:
  reactor_webrtc_encoded_frame_cb cb_;
  void* userdata_;
  reactor_webrtc_userdata_free free_ud_;
  std::mutex mu_;
  webrtc::scoped_refptr<webrtc::TransformedFrameCallback> send_sink_;
  std::map<uint32_t, webrtc::scoped_refptr<webrtc::TransformedFrameCallback>>
      recv_sinks_;
};

// Owns one reference to the transformer (the sender/receiver keep their own).
struct ReactorTransformerHandle {
  webrtc::scoped_refptr<ReactorFrameTransformer> t;
};

// Create an encoded-frame transformer. `cb` fires per encoded frame (see
// ReactorEncodedFrame); `free_ud` (nullable) frees `userdata` when the
// transformer is finally destroyed. Returns an owned handle
// (free with reactor_webrtc_frame_transformer_destroy) or null.
void* reactor_webrtc_frame_transformer_create(
    reactor_webrtc_encoded_frame_cb cb, void* userdata,
    reactor_webrtc_userdata_free free_ud) {
  auto t = webrtc::make_ref_counted<ReactorFrameTransformer>(cb, userdata,
                                                             free_ud);
  return new ReactorTransformerHandle{std::move(t)};
}

// Attach the transformer to the transceiver's sender (encoder -> packetizer).
// Returns 1 on success, 0 on failure.
int reactor_webrtc_rtp_transceiver_set_sender_transform(void* transceiver,
                                                        void* transformer) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  auto* th = reinterpret_cast<ReactorTransformerHandle*>(transformer);
  if (!h || !h->tc || !th || !h->tc->sender()) return 0;
  h->tc->sender()->SetFrameTransformer(th->t);
  return 1;
}

// Attach the transformer to the transceiver's receiver (depacketizer ->
// decoder). Returns 1 on success, 0 on failure.
int reactor_webrtc_rtp_transceiver_set_receiver_transform(void* transceiver,
                                                          void* transformer) {
  auto* h = reinterpret_cast<ReactorTransceiver*>(transceiver);
  auto* th = reinterpret_cast<ReactorTransformerHandle*>(transformer);
  if (!h || !h->tc || !th || !h->tc->receiver()) return 0;
  h->tc->receiver()->SetFrameTransformer(th->t);
  return 1;
}

// Release a transformer handle (our reference; sender/receiver keep theirs).
void reactor_webrtc_frame_transformer_destroy(void* transformer) {
  delete reinterpret_cast<ReactorTransformerHandle*>(transformer);
}

// Identity of the native track behind this handle, as an opaque value — not an
// owning handle, on the same terms as
// reactor_webrtc_rtp_transceiver_sender_track_id, whose value this is comparable
// with. Returns 0 for a handle with no track.
uintptr_t reactor_webrtc_media_stream_track_id(void* track) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (!h) return 0;
  return reinterpret_cast<uintptr_t>(h->track.get());
}

// Destroy a track handle (detaches any sink and releases the track + source).
void reactor_webrtc_media_stream_track_destroy(void* track) {
  auto* h = reinterpret_cast<ReactorMediaStreamTrack*>(track);
  if (h && h->track) {
    if (h->sink && h->track->kind() == "video") {
      static_cast<webrtc::VideoTrackInterface*>(h->track.get())
          ->RemoveSink(h->sink.get());
    }
    if (h->audio_sink && h->track->kind() == "audio") {
      static_cast<webrtc::AudioTrackInterface*>(h->track.get())
          ->RemoveSink(h->audio_sink.get());
    }
  }
  delete h;
}

// ── Stats ─────────────────────────────────────────────────────────────────────

// Request a stats snapshot from the peer connection. `callback(userdata,
// entries, count)` fires once on the WebRTC signaling thread — `entries` is a
// pointer to a temporary array valid only for the duration of the call. If
// the peer connection is closed or not set, `callback` is called with count=0.
void reactor_webrtc_peer_connection_get_stats(
    void* pc, void* userdata,
    void (*callback)(void* userdata, const ReactorStatEntry* entries, int count)) {
  auto* rpc = reinterpret_cast<ReactorPeerConnection*>(pc);
  if (!rpc || !rpc->pc) {
    if (callback) callback(userdata, nullptr, 0);
    return;
  }
  // Take an extra ref before GetStats() sees this object, released at the end
  // of OnStatsDelivered — see the comment there for why. `cb` itself is a
  // second, independent reference that still unwinds normally at the end of
  // this function.
  webrtc::scoped_refptr<StatsCallback> cb =
      webrtc::make_ref_counted<StatsCallback>(userdata, callback);
  cb->AddRef();
  rpc->pc->GetStats(cb.get());
}

}  // extern "C"

// ── Android bootstrap ─────────────────────────────────────────────────────────
// Called from JNI_OnLoad (reactor-ffi/src/lib.rs) to hand the JavaVM to
// libwebrtc before any PeerConnectionFactory is created. The Java classes are
// namespaced inc.reactor.org.webrtc.* via android_jni_package_prefix="inc.reactor"
// (webrtc-build/patches/0002-*); JNI_OnLoad in libwebrtc's jni_onload.cc wires
// them up through InitClassLoader.
//
// reactor_webrtc_android_init_context is provided for completeness (platform ADM
// needs the Application Context), but the synthetic ADM used on Android does not
// require it — so it simply re-initialises the JavaVM and returns 1 (success).

#ifdef WEBRTC_ANDROID
#include <jni.h>
#include "sdk/android/native_api/base/init.h"

extern "C" {

void reactor_webrtc_android_init(void* vm) {
  webrtc::InitAndroid(static_cast<JavaVM*>(vm));
}

int reactor_webrtc_android_init_context(void* vm, void* /*context*/) {
  webrtc::InitAndroid(static_cast<JavaVM*>(vm));
  return 1;
}

}  // extern "C"
#endif  // WEBRTC_ANDROID
