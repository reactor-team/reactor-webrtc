//! What [`VideoCodecs::capabilities`](super::VideoCodecs::capabilities)
//! reports.

use super::{c_name, VideoCodec};

/// One backend for one codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecSupport {
    pub codec: VideoCodec,
    /// Whether the backend is the platform's hardware codec. VideoToolbox
    /// counts as hardware, as libwebrtc reports it; on a Mac with no
    /// hardware encoder (a virtual machine) it runs Apple's software one.
    pub hardware: bool,
    /// The backend's name, e.g. `"VideoToolbox"` or `"OpenH264"`.
    pub implementation: String,
}

/// The codecs a process can encode and decode, one entry per backend, hardware
/// first within each codec. A codec missing from a list cannot be created in
/// that direction here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodecCapabilities {
    pub encoders: Vec<CodecSupport>,
    pub decoders: Vec<CodecSupport>,
}

impl CodecCapabilities {
    /// Whether some backend encodes `codec`.
    pub fn can_encode(&self, codec: VideoCodec) -> bool {
        self.encoders.iter().any(|s| s.codec == codec)
    }

    /// Whether some backend decodes `codec`.
    pub fn can_decode(&self, codec: VideoCodec) -> bool {
        self.decoders.iter().any(|s| s.codec == codec)
    }
}

pub(super) fn read(ctx: *mut reactor_webrtc_sys::CodecContext) -> CodecCapabilities {
    let count = unsafe {
        reactor_webrtc_sys::reactor_webrtc_codec_context_capabilities(ctx, std::ptr::null_mut(), 0)
    };
    let mut raw = vec![
        reactor_webrtc_sys::ReactorCodecSupport {
            direction: 0,
            codec: 0,
            hardware: 0,
            implementation: [0; 64],
        };
        count.max(0) as usize
    ];
    let written = unsafe {
        reactor_webrtc_sys::reactor_webrtc_codec_context_capabilities(
            ctx,
            raw.as_mut_ptr(),
            raw.len() as i32,
        )
    };
    raw.truncate(written.clamp(0, count.max(0)) as usize);
    let mut caps = CodecCapabilities::default();
    for s in raw {
        let Some(codec) = VideoCodec::from_u32(s.codec as u32) else {
            continue;
        };
        let support = CodecSupport {
            codec,
            hardware: s.hardware != 0,
            implementation: c_name(&s.implementation),
        };
        if s.direction == 0 {
            caps.encoders.push(support);
        } else {
            caps.decoders.push(support);
        }
    }
    caps
}
