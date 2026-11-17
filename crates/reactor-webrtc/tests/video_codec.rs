//! The standalone video codecs: a sequence encoded and decoded back matches
//! within tolerance, a key frame comes out when asked mid-stream, and a reset
//! decoder starts again only at a key frame — for every codec, on every
//! backend this machine has, hardware and software.
//!
//! The software H.264 backend is OpenH264, so it runs only with the `openh264`
//! feature on a platform Cisco ships it for (Linux, Windows); the library is
//! downloaded on first use. VideoToolbox covers the hardware H.264 path on
//! Apple. VP8, VP9 and AV1 run on libwebrtc's own software codecs everywhere.
//!
//! Gated on a native libwebrtc being linked (see build.rs).
#![cfg(have_libwebrtc)]

use reactor_webrtc::codec::{
    Backend, CodecError, CodecSupport, EncodedFrame, VideoCodecs, VideoDecoder, VideoEncoder,
    VideoEncoderConfig,
};
use reactor_webrtc::{PixelFormat, VideoCodec};

const W: u32 = 640;
const H: u32 = 360;
/// Frame timestamps, 30 fps apart.
const FRAME_US: i64 = 33_333;
/// Lowest PSNR a decoded frame may have against its source, over the RGB
/// bytes. The I420 round trip alone costs a few dB on this pattern's colour
/// edges (VideoToolbox lands at 32-42 dB); a broken conversion (swapped
/// channels, a wrong stride) lands far below.
const MIN_PSNR_DB: f64 = 28.0;

/// Every codec the standalone API runs.
const CODECS: [VideoCodec; 4] = [
    VideoCodec::H264,
    VideoCodec::Vp8,
    VideoCodec::Vp9,
    VideoCodec::Av1,
];

/// The codecs with every backend this platform can load.
fn codecs() -> VideoCodecs {
    #[cfg(feature = "openh264")]
    {
        match reactor_webrtc::openh264::ensure_available(None) {
            Ok(path) => {
                return VideoCodecs::with_openh264(&path).expect("codecs with OpenH264");
            }
            Err(e) => eprintln!("OpenH264 unavailable here ({e}); platform codecs only"),
        }
    }
    VideoCodecs::new().expect("codecs")
}

/// The (codec, backend) pairs to run each test on: every codec, on each kind
/// of backend that both encodes and decodes it here.
fn cases(codecs: &VideoCodecs) -> Vec<(VideoCodec, Backend)> {
    let caps = codecs.capabilities();
    let mut out = Vec::new();
    for codec in CODECS {
        let mut found = false;
        for (backend, hardware) in [(Backend::Hardware, true), (Backend::Software, false)] {
            let has = |list: &[CodecSupport]| {
                list.iter()
                    .any(|s| s.codec == codec && s.hardware == hardware)
            };
            if has(&caps.encoders) && has(&caps.decoders) {
                out.push((codec, backend));
                found = true;
            }
        }
        if !found {
            eprintln!("no {codec:?} backend on this machine; skipping it");
        }
    }
    out
}

fn open(codecs: &VideoCodecs, codec: VideoCodec, backend: Backend) -> (VideoEncoder, VideoDecoder) {
    let mut config = VideoEncoderConfig::new(codec, W, H);
    config.backend = backend;
    let encoder = codecs.encoder(&config).expect("encoder");
    let decoder = codecs.decoder_on(codec, backend).expect("decoder");
    assert_eq!(encoder.codec(), codec);
    assert_eq!(decoder.codec(), codec);
    assert_eq!(encoder.is_hardware(), backend == Backend::Hardware);
    assert_eq!(decoder.is_hardware(), backend == Backend::Hardware);
    (encoder, decoder)
}

/// Frame `i` of a smooth moving pattern, RGB24: diagonal gradients with a
/// bright square that walks across the picture.
fn frame(i: u32) -> Vec<u8> {
    let mut px = vec![0u8; (W * H * 3) as usize];
    let (sx, sy) = ((i * 8) % (W - 64), (i * 4) % (H - 64));
    for y in 0..H {
        for x in 0..W {
            let o = ((y * W + x) * 3) as usize;
            let inside = x >= sx && x < sx + 64 && y >= sy && y < sy + 64;
            if inside {
                px[o..o + 3].copy_from_slice(&[240, 220, 40]);
            } else {
                px[o] = ((x + i * 2) * 255 / W) as u8;
                px[o + 1] = ((y + i) * 255 / H) as u8;
                px[o + 2] = (((x + y) / 2) * 255 / ((W + H) / 2)) as u8;
            }
        }
    }
    px
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

fn encode(encoder: &mut VideoEncoder, i: u32) -> EncodedFrame {
    encoder
        .encode(&frame(i), W, H, PixelFormat::Rgb24, i as i64 * FRAME_US)
        .expect("encode")
        .expect("the backend dropped a frame")
}

/// Decode `encoded`, the encoding of frame `i`, and check it against the
/// source.
fn decode_and_compare(decoder: &mut VideoDecoder, encoded: &EncodedFrame, i: u32) {
    let decoded = decoder
        .decode(&encoded.data, encoded.timestamp_us, PixelFormat::Rgb24)
        .expect("decode")
        .expect("a whole frame gave no picture");
    assert_eq!((decoded.width, decoded.height), (W, H));
    assert_eq!(decoded.timestamp_us, encoded.timestamp_us);
    let quality = psnr(&decoded.data, &frame(i));
    assert!(
        quality >= MIN_PSNR_DB,
        "{:?} on {}: frame {i} decoded at {quality:.1} dB",
        decoder.codec(),
        decoder.implementation()
    );
}

#[test]
fn a_sequence_round_trips_within_tolerance() {
    let codecs = codecs();
    for (codec, backend) in cases(&codecs) {
        let (mut encoder, mut decoder) = open(&codecs, codec, backend);
        let mut deltas = 0;
        for i in 0..30 {
            let encoded = encode(&mut encoder, i);
            assert_eq!(encoded.timestamp_us, i as i64 * FRAME_US);
            assert_eq!((encoded.width, encoded.height), (W, H));
            if i == 0 {
                assert!(
                    encoded.key_frame,
                    "{codec:?} {backend:?}: the first frame is not a key frame"
                );
            } else if !encoded.key_frame {
                deltas += 1;
            }
            decode_and_compare(&mut decoder, &encoded, i);
        }
        // Inter frames, not a stream of key frames.
        assert!(
            deltas >= 25,
            "{codec:?} on {}: only {deltas} of 29 frames were inter frames",
            encoder.implementation()
        );
    }
}

#[test]
fn a_key_frame_comes_out_on_demand_mid_stream() {
    let codecs = codecs();
    for (codec, backend) in cases(&codecs) {
        let (mut encoder, mut decoder) = open(&codecs, codec, backend);
        for i in 0..10 {
            let encoded = encode(&mut encoder, i);
            decode_and_compare(&mut decoder, &encoded, i);
        }
        encoder.request_key_frame();
        let key = encode(&mut encoder, 10);
        assert!(
            key.key_frame,
            "{codec:?} {backend:?}: no key frame after request_key_frame"
        );
        let next = encode(&mut encoder, 11);
        assert!(
            !next.key_frame,
            "{codec:?} {backend:?}: the request outlived its frame"
        );

        // The stream so far decodes through the key frame…
        decode_and_compare(&mut decoder, &key, 10);
        decode_and_compare(&mut decoder, &next, 11);
        // …and a decoder that joins at it needs nothing before it.
        let mut late = codecs.decoder_on(codec, backend).expect("decoder");
        decode_and_compare(&mut late, &key, 10);
        decode_and_compare(&mut late, &next, 11);
    }
}

#[test]
fn a_reset_decoder_starts_again_only_at_a_key_frame() {
    let codecs = codecs();
    for (codec, backend) in cases(&codecs) {
        let (mut encoder, mut decoder) = open(&codecs, codec, backend);
        let stream: Vec<EncodedFrame> = (0..6).map(|i| encode(&mut encoder, i)).collect();
        assert!(stream[0].key_frame, "{codec:?} {backend:?}");
        assert!(
            stream[1..].iter().all(|f| !f.key_frame),
            "{codec:?} {backend:?}"
        );

        // A new decoder refuses an inter frame without touching its state.
        let mut fresh = codecs.decoder_on(codec, backend).expect("decoder");
        assert_eq!(
            fresh.decode(&stream[1].data, 0, PixelFormat::Rgb24),
            Err(CodecError::NeedKeyFrame),
            "{codec:?} {backend:?}"
        );
        assert!(fresh.needs_key_frame());

        for (i, f) in stream[..3].iter().enumerate() {
            decode_and_compare(&mut decoder, f, i as u32);
        }
        assert!(!decoder.needs_key_frame());
        decoder.reset().expect("reset");
        assert!(decoder.needs_key_frame());
        for f in &stream[3..] {
            assert_eq!(
                decoder.decode(&f.data, f.timestamp_us, PixelFormat::Rgb24),
                Err(CodecError::NeedKeyFrame),
                "{codec:?} {backend:?}: a reset decoder took an inter frame"
            );
        }

        // The encoder's next key frame brings it back.
        encoder.request_key_frame();
        let key = encode(&mut encoder, 6);
        assert!(key.key_frame, "{codec:?} {backend:?}");
        decode_and_compare(&mut decoder, &key, 6);
        assert!(!decoder.needs_key_frame());
        let next = encode(&mut encoder, 7);
        decode_and_compare(&mut decoder, &next, 7);
    }
}

#[test]
fn a_new_frame_size_starts_with_a_key_frame() {
    let codecs = codecs();
    for (codec, backend) in cases(&codecs) {
        let (mut encoder, _) = open(&codecs, codec, backend);
        encode(&mut encoder, 0);
        assert!(!encode(&mut encoder, 1).key_frame, "{codec:?} {backend:?}");
        // Larger than the level in the SDP format the H.264 backends are
        // created with: the encoder must not hold the stream to it.
        let (w, h) = (1920, 1080);
        let large = encoder
            .encode(
                &vec![128u8; (w * h * 3) as usize],
                w,
                h,
                PixelFormat::Rgb24,
                0,
            )
            .expect("encode")
            .expect("frame");
        assert!(
            large.key_frame,
            "{codec:?} {backend:?}: a new size without a key frame"
        );
        assert_eq!((large.width, large.height), (w, h));
        let mut decoder = codecs.decoder_on(codec, backend).expect("decoder");
        let decoded = decoder
            .decode(&large.data, 0, PixelFormat::Bgra)
            .expect("decode")
            .expect("picture");
        assert_eq!((decoded.width, decoded.height), (w, h));
        assert_eq!(decoded.data.len(), (w * h * 4) as usize);
    }
}

#[test]
fn an_encoder_recovers_from_a_frame_size_it_refused() {
    let codecs = codecs();
    // H.264 only: VideoToolbox is the backend that refuses a size at re-init,
    // and a frame this large is slow on the software VP9/AV1 encoders.
    for (codec, backend) in cases(&codecs)
        .into_iter()
        .filter(|(codec, _)| *codec == VideoCodec::H264)
    {
        let (mut encoder, _) = open(&codecs, codec, backend);
        encode(&mut encoder, 0);
        // Above every level the backends are set up for: VideoToolbox refuses
        // it at re-init, after releasing the session for the old size.
        let (w, h) = (4352, 4352);
        let big = encoder.encode(
            &vec![128u8; (w * h * 3) as usize],
            w,
            h,
            PixelFormat::Rgb24,
            FRAME_US,
        );
        if big.is_ok() {
            eprintln!(
                "{}: took {w}x{h}; the failed re-init is not exercised",
                encoder.implementation()
            );
        }
        // Back to the old size, the encoder works again from a key frame.
        let encoded = encoder
            .encode(&frame(2), W, H, PixelFormat::Rgb24, 2 * FRAME_US)
            .unwrap_or_else(|e| panic!("{backend:?}: back to {W}x{H}: {e}"))
            .expect("the backend dropped a frame");
        assert!(
            encoded.key_frame,
            "{backend:?}: no key frame after the size change"
        );
        assert_eq!((encoded.width, encoded.height), (W, H));
    }
}

#[test]
fn capabilities_list_h264_where_a_backend_exists() {
    let codecs = codecs();
    let caps = codecs.capabilities();
    let expect_hardware = cfg!(target_vendor = "apple");
    let expect_software = cfg!(feature = "openh264")
        && (cfg!(target_os = "linux") || cfg!(target_os = "windows"))
        && (cfg!(target_env = "gnu") || cfg!(target_env = "msvc"));
    for (list, what) in [(&caps.encoders, "encoders"), (&caps.decoders, "decoders")] {
        let hw = list
            .iter()
            .find(|s| s.codec == VideoCodec::H264 && s.hardware);
        let sw = list
            .iter()
            .find(|s| s.codec == VideoCodec::H264 && !s.hardware);
        if expect_hardware {
            assert_eq!(
                hw.map(|s| s.implementation.as_str()),
                Some("VideoToolbox"),
                "{what}: {caps:?}"
            );
        }
        if expect_software {
            assert_eq!(
                sw.map(|s| s.implementation.as_str()),
                Some("OpenH264"),
                "{what}: {caps:?}"
            );
        }
    }
    assert_eq!(
        caps.can_encode(VideoCodec::H264),
        caps.can_decode(VideoCodec::H264)
    );
    if !caps.can_encode(VideoCodec::H264) {
        assert!(!expect_hardware && !expect_software);
    }
}

#[test]
fn capabilities_list_the_builtin_software_codecs() {
    let caps = codecs().capabilities();
    // libvpx is part of every libwebrtc build this crate links.
    for codec in [VideoCodec::Vp8, VideoCodec::Vp9] {
        for (list, what) in [(&caps.encoders, "encoders"), (&caps.decoders, "decoders")] {
            let entry = list.iter().find(|s| s.codec == codec);
            assert_eq!(
                entry.map(|s| (s.implementation.as_str(), s.hardware)),
                Some(("libvpx", false)),
                "{codec:?} {what}: {caps:?}"
            );
        }
    }
    // AV1 is listed in each direction the build has, under its library.
    if let Some(s) = caps.encoders.iter().find(|s| s.codec == VideoCodec::Av1) {
        assert_eq!((s.implementation.as_str(), s.hardware), ("libaom", false));
    }
    if let Some(s) = caps.decoders.iter().find(|s| s.codec == VideoCodec::Av1) {
        assert_eq!((s.implementation.as_str(), s.hardware), ("dav1d", false));
    }
}

#[test]
fn capabilities_group_codecs_hardware_first() {
    let caps = codecs().capabilities();
    for (list, what) in [(&caps.encoders, "encoders"), (&caps.decoders, "decoders")] {
        for codec in CODECS {
            let of_codec: Vec<&CodecSupport> = list.iter().filter(|s| s.codec == codec).collect();
            if let (Some(h), Some(s)) = (
                of_codec.iter().position(|s| s.hardware),
                of_codec.iter().position(|s| !s.hardware),
            ) {
                assert!(
                    h < s,
                    "{what}: {codec:?} software listed before hardware: {caps:?}"
                );
            }
        }
    }
}

#[test]
fn a_codec_missing_from_the_capabilities_is_refused_without_a_panic() {
    let codecs = codecs();
    let caps = codecs.capabilities();
    // H.265 never runs standalone.
    assert!(!caps.can_encode(VideoCodec::H265));
    assert!(!caps.can_decode(VideoCodec::H265));
    for codec in CODECS.into_iter().chain([VideoCodec::H265]) {
        if !caps.can_encode(codec) {
            let config = VideoEncoderConfig::new(codec, W, H);
            assert!(
                matches!(codecs.encoder(&config), Err(CodecError::Unsupported(_))),
                "{codec:?} encoder: {caps:?}"
            );
        }
        if !caps.can_decode(codec) {
            assert!(
                matches!(codecs.decoder(codec), Err(CodecError::Unsupported(_))),
                "{codec:?} decoder: {caps:?}"
            );
        }
        // A backend kind the codec lacks here is refused the same way.
        for (backend, hardware) in [(Backend::Hardware, true), (Backend::Software, false)] {
            if !caps
                .encoders
                .iter()
                .any(|s| s.codec == codec && s.hardware == hardware)
            {
                let mut config = VideoEncoderConfig::new(codec, W, H);
                config.backend = backend;
                assert!(
                    matches!(codecs.encoder(&config), Err(CodecError::Unsupported(_))),
                    "{codec:?} {backend:?} encoder: {caps:?}"
                );
            }
            if !caps
                .decoders
                .iter()
                .any(|s| s.codec == codec && s.hardware == hardware)
            {
                assert!(
                    matches!(
                        codecs.decoder_on(codec, backend),
                        Err(CodecError::Unsupported(_))
                    ),
                    "{codec:?} {backend:?} decoder: {caps:?}"
                );
            }
        }
    }
}

#[test]
fn bad_input_is_refused_before_the_codec() {
    let codecs = codecs();
    let h265 = VideoEncoderConfig::new(VideoCodec::H265, W, H);
    assert!(matches!(
        codecs.encoder(&h265),
        Err(CodecError::Unsupported(_))
    ));
    for (codec, backend) in cases(&codecs) {
        let odd = VideoEncoderConfig::new(codec, 641, H);
        assert!(matches!(
            codecs.encoder(&odd),
            Err(CodecError::InvalidInput(_))
        ));
        let (mut encoder, mut decoder) = open(&codecs, codec, backend);
        assert!(matches!(
            encoder.encode(&[0u8; 10], W, H, PixelFormat::Rgb24, 0),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(matches!(
            decoder.decode(&[], 0, PixelFormat::Rgb24),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(encoder.set_rates(0, 30.0).is_err());
        assert!(encoder.set_rates(1_000_000, 1e-300).is_err());
        let mut slow = VideoEncoderConfig::new(codec, W, H);
        slow.framerate = 1e-300;
        assert!(matches!(
            codecs.encoder(&slow),
            Err(CodecError::InvalidInput(_))
        ));
        encoder.set_rates(1_000_000, 15.0).expect("set_rates");
        assert!(encode(&mut encoder, 0).key_frame, "{codec:?} {backend:?}");
    }
}
