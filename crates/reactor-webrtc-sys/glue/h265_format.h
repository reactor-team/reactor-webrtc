// The H.265 SDP format the NVENC encoders, the NVDEC decoders and the
// standalone codecs list, as Chrome lists its own: Main profile (profile-id
// 1), Main tier, single RTP stream (tx-mode SRST), at level 5.2 (level-id is
// 30 times the level), which takes 4096x2176 at 60 fps. libwebrtc built with
// rtc_use_h265 matches formats by profile, tier and tx-mode, and answers the
// lower of the two levels.
#pragma once

#include "api/video_codecs/sdp_video_format.h"

namespace reactor {

inline webrtc::SdpVideoFormat H265MainFormat() {
  return webrtc::SdpVideoFormat("H265", {{"profile-id", "1"},
                                         {"tier-flag", "0"},
                                         {"level-id", "156"},
                                         {"tx-mode", "SRST"}});
}

}  // namespace reactor
