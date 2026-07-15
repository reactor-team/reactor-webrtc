//! [`EncodedVideoBuilder`] — factory + mix of raw and pre-encoded video tracks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::encoded::{EncodedVideoFrame, EncodedVideoTrack, EncoderRegistry};
use crate::media::Track;
use crate::{CustomVideoEncoder, PeerConnectionFactory, Result};

/// One video track produced by [`EncodedVideoBuilder::build`].
///
/// - [`Raw`](MixedVideoTrack::Raw) — push BGRA frames with
///   [`Track::push_video_frame`]; libwebrtc encodes to VP8/VP9/AV1.
/// - [`Encoded`](MixedVideoTrack::Encoded) — push pre-encoded bytes with
///   [`EncodedVideoTrack::push_encoded_frame`]; libwebrtc only packetises.
pub enum MixedVideoTrack {
    Raw(Track),
    Encoded(EncodedVideoTrack),
}

impl MixedVideoTrack {
    /// The underlying [`Track`] handle — attach this to a transceiver with
    /// [`Transceiver::set_track`](crate::Transceiver::set_track).
    pub fn track(&self) -> &Track {
        match self {
            Self::Raw(t) => t,
            Self::Encoded(e) => e.track(),
        }
    }

    /// Returns the inner [`EncodedVideoTrack`], or `None` if this is a raw track.
    pub fn as_encoded(&self) -> Option<&EncodedVideoTrack> {
        match self {
            Self::Encoded(e) => Some(e),
            Self::Raw(_) => None,
        }
    }

    /// Returns the inner raw [`Track`], or `None` if this is a pre-encoded track.
    pub fn as_raw(&self) -> Option<&Track> {
        match self {
            Self::Raw(t) => Some(t),
            Self::Encoded(_) => None,
        }
    }
}

enum SlotConfig {
    Raw {
        id: String,
    },
    Encoded {
        id: String,
        width: u32,
        height: u32,
        queue: Arc<Mutex<VecDeque<EncodedVideoFrame>>>,
    },
}

/// Builds a [`PeerConnectionFactory`] wired for **multiple** video tracks that
/// may be a mix of raw (builtin encoder) and pre-encoded (custom encoder).
///
/// ```rust,ignore
/// let mut b = PeerConnectionFactory::encoded_video_builder();
///
/// // Raw track: push BGRA → libwebrtc encodes → RTP
/// let cam_idx = b.add_raw_track("camera", 1280, 720);
///
/// // Pre-encoded track: push your own bytes → RTP
/// let scr_idx = b.add_encoded_track("screen", 1920, 1080);
///
/// let (factory, tracks) = b.build()?;
///
/// if let MixedVideoTrack::Raw(cam) = &tracks[cam_idx] {
///     cam.push_video_frame(&bgra, 1280, 720);
/// }
/// if let MixedVideoTrack::Encoded(scr) = &tracks[scr_idx] {
///     scr.push_encoded_frame(screen_frame);
/// }
/// ```
///
/// For a single pre-encoded track the convenience method
/// [`PeerConnectionFactory::with_encoded_video_track`] is simpler.
pub struct EncodedVideoBuilder {
    registry: Arc<EncoderRegistry>,
    slots: Vec<SlotConfig>,
}

impl EncodedVideoBuilder {
    pub(crate) fn new() -> Self {
        Self {
            registry: EncoderRegistry::new(),
            slots: Vec::new(),
        }
    }

    /// Add a **raw** video track. Push BGRA frames with
    /// [`Track::push_video_frame`]; libwebrtc's builtin VP8/VP9/AV1 encoder
    /// handles compression. No `width`/`height` needed here — set them per frame.
    ///
    /// Returns the index into the `Vec<MixedVideoTrack>` that [`build`] produces.
    pub fn add_raw_track(&mut self, id: &str) -> usize {
        self.registry.add_raw_slot();
        let idx = self.slots.len();
        self.slots.push(SlotConfig::Raw { id: id.to_owned() });
        idx
    }

    /// Add a **pre-encoded** video track. Push encoded bytes with
    /// [`EncodedVideoTrack::push_encoded_frame`]; libwebrtc only packetises.
    ///
    /// Returns the index into the `Vec<MixedVideoTrack>` that [`build`] produces.
    pub fn add_encoded_track(&mut self, id: &str, width: u32, height: u32) -> usize {
        let queue = self.registry.add_encoded_slot();
        let idx = self.slots.len();
        self.slots.push(SlotConfig::Encoded {
            id: id.to_owned(),
            width,
            height,
            queue,
        });
        idx
    }

    /// Convenience alias for [`add_encoded_track`] (backward compat).
    pub fn add_track(&mut self, id: &str, width: u32, height: u32) -> usize {
        self.add_encoded_track(id, width, height)
    }

    /// Finalise the factory and create all registered video tracks.
    ///
    /// Returns `(factory, tracks)` where `tracks[i]` corresponds to the i-th
    /// `add_*_track` call.
    pub fn build(self) -> Result<(PeerConnectionFactory, Vec<MixedVideoTrack>)> {
        let encoder = CustomVideoEncoder::from_registry(self.registry);
        let factory = PeerConnectionFactory::with_custom_video_encoder(encoder)?;
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in self.slots {
            match slot {
                SlotConfig::Raw { id } => {
                    let track = factory.create_video_track(&id)?;
                    out.push(MixedVideoTrack::Raw(track));
                }
                SlotConfig::Encoded {
                    id,
                    width,
                    height,
                    queue,
                } => {
                    let track = factory.create_video_track(&id)?;
                    out.push(MixedVideoTrack::Encoded(EncodedVideoTrack::new(
                        track, queue, width, height,
                    )));
                }
            }
        }
        Ok((factory, out))
    }
}
