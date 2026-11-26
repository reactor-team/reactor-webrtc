"""The standalone video codecs through the Python binding.

For every codec this process can both encode and decode: a sequence encoded and
decoded back matches its source within tolerance, a key frame comes out when
asked mid-stream and is marked as one, and a reset decoder starts again only at
a key frame. Frames go in and come out as numpy uint8 arrays of shape
(H, W, 3). A codec with no backend here (H.264 on Linux without OpenH264, say)
is skipped, not failed.

None of this touches a PeerConnectionFactory, so it sits beside the session
factory in conftest without creating another.
"""

from __future__ import annotations

import threading

import numpy as np
import pytest
import reactor_webrtc as rw

W, H = 320, 180
FRAME_US = 33_333
# Lowest PSNR a decoded frame may have against its source. The I420 round trip
# alone costs a few dB on the pattern's colour edges; a broken conversion
# (swapped channels, a wrong stride or shape) lands far below.
MIN_PSNR_DB = 28.0

CODECS = [rw.VideoCodec.H264, rw.VideoCodec.Vp8, rw.VideoCodec.Vp9, rw.VideoCodec.Av1]


@pytest.fixture(scope="module")
def codecs() -> rw.VideoCodecs:
    return rw.VideoCodecs()


def _require(codecs: rw.VideoCodecs, codec: rw.VideoCodec) -> None:
    caps = codecs.capabilities()
    if not (caps.can_encode(codec) and caps.can_decode(codec)):
        pytest.skip(f"no {codec} encoder and decoder on this machine")


def frame(i: int) -> np.ndarray:
    """Frame `i` of a smooth moving pattern: gradients and a walking square."""
    y, x = np.mgrid[0:H, 0:W]
    px = np.empty((H, W, 3), dtype=np.uint8)
    px[..., 0] = ((x + i * 2) * 255 // W) % 256
    px[..., 1] = ((y + i) * 255 // H) % 256
    px[..., 2] = ((x + y) // 2) * 255 // ((W + H) // 2)
    sx, sy = (i * 8) % (W - 32), (i * 4) % (H - 32)
    px[sy : sy + 32, sx : sx + 32] = (240, 220, 40)
    return px


def psnr(a: np.ndarray, b: np.ndarray) -> float:
    mse = np.mean((a.astype(np.float64) - b.astype(np.float64)) ** 2)
    return float("inf") if mse == 0 else 10 * np.log10(255.0**2 / mse)


# ── Capabilities and types ────────────────────────────────────────────────────


def test_capabilities_list_supports(codecs: rw.VideoCodecs) -> None:
    caps = codecs.capabilities()
    assert isinstance(caps, rw.CodecCapabilities)
    for s in caps.encoders + caps.decoders:
        assert isinstance(s, rw.CodecSupport)
        assert isinstance(s.codec, rw.VideoCodec)
        assert isinstance(s.hardware, bool)
        assert s.implementation
    for codec in CODECS:
        assert caps.can_encode(codec) == any(s.codec == codec for s in caps.encoders)
        assert caps.can_decode(codec) == any(s.codec == codec for s in caps.decoders)
    assert "CodecCapabilities" in repr(caps)


def test_exceptions_form_a_hierarchy() -> None:
    assert issubclass(rw.CodecError, RuntimeError)
    assert issubclass(rw.CodecUnsupported, rw.CodecError)
    assert issubclass(rw.CodecNeedsKeyFrame, rw.CodecError)


def test_h265_has_no_backend_without_nvenc_and_nvdec(codecs: rw.VideoCodecs) -> None:
    # H.265 runs only on NVENC and NVDEC, which these codecs did not ask for.
    with pytest.raises(rw.CodecUnsupported):
        codecs.encoder(rw.VideoCodec.H265, W, H)
    with pytest.raises(rw.CodecUnsupported):
        codecs.decoder(rw.VideoCodec.H265)


def test_bad_encoder_settings_raise_value_error(codecs: rw.VideoCodecs) -> None:
    _require(codecs, rw.VideoCodec.Vp8)
    with pytest.raises(ValueError):
        codecs.encoder(rw.VideoCodec.Vp8, W + 1, H)
    with pytest.raises(ValueError):
        codecs.encoder(rw.VideoCodec.Vp8, W, H, bitrate_bps=0)
    with pytest.raises(ValueError):
        codecs.encoder(rw.VideoCodec.Vp8, W, H, framerate=0.0)
    encoder = codecs.encoder(rw.VideoCodec.Vp8, W, H)
    with pytest.raises(ValueError):
        encoder.set_rates(0, 30.0)


@pytest.mark.parametrize("value", [-1, -W, 2**32])
def test_out_of_range_settings_raise_value_error(
    codecs: rw.VideoCodecs, value: int
) -> None:
    # Not OverflowError: a negative or too large size or bitrate is a bad
    # setting like any other.
    _require(codecs, rw.VideoCodec.Vp8)
    with pytest.raises(ValueError):
        codecs.encoder(rw.VideoCodec.Vp8, value, H)
    with pytest.raises(ValueError):
        codecs.encoder(rw.VideoCodec.Vp8, W, value)
    with pytest.raises(ValueError):
        codecs.encoder(rw.VideoCodec.Vp8, W, H, bitrate_bps=value)
    encoder = codecs.encoder(rw.VideoCodec.Vp8, W, H)
    with pytest.raises(ValueError):
        encoder.set_rates(value, 30.0)


def test_encode_checks_the_frame(codecs: rw.VideoCodecs) -> None:
    _require(codecs, rw.VideoCodec.Vp8)
    encoder = codecs.encoder(rw.VideoCodec.Vp8, W, H)
    with pytest.raises(TypeError):
        encoder.encode(frame(0).tobytes())
    with pytest.raises(ValueError, match="dtype"):
        encoder.encode(frame(0).astype(np.float32))
    with pytest.raises(ValueError, match="shape"):
        encoder.encode(np.zeros((H, W, 4), dtype=np.uint8))
    with pytest.raises(ValueError, match="shape"):
        encoder.encode(np.zeros((H, W), dtype=np.uint8))
    with pytest.raises(ValueError):
        encoder.encode(np.zeros((H + 1, W, 3), dtype=np.uint8))


# ── Per codec ─────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("codec", CODECS, ids=lambda c: str(c).rsplit(".", 1)[-1])
def test_round_trip_matches_the_source(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    _require(codecs, codec)
    encoder = codecs.encoder(codec, W, H, bitrate_bps=2_000_000, framerate=30.0)
    decoder = codecs.decoder(codec)
    assert encoder.codec == codec and decoder.codec == codec
    assert encoder.implementation and decoder.implementation
    assert encoder.bitrate_bps == 2_000_000 and encoder.framerate == 30.0
    assert decoder.needs_key_frame

    deltas = 0
    for i in range(10):
        src = frame(i)
        encoded = encoder.encode(src, i * FRAME_US)
        assert encoded is not None, f"frame {i} dropped"
        assert isinstance(encoded.data, bytes) and encoded.data
        assert (encoded.width, encoded.height) == (W, H)
        assert encoded.timestamp_us == i * FRAME_US
        if i == 0:
            assert encoded.key_frame, "the first frame is not a key frame"
        elif not encoded.key_frame:
            deltas += 1

        picture = decoder.decode(encoded.data)
        assert picture is not None
        assert picture.dtype == np.uint8 and picture.shape == (H, W, 3)
        assert picture.flags.writeable
        assert psnr(src, picture) >= MIN_PSNR_DB, (
            f"frame {i}: {psnr(src, picture):.1f} dB"
        )
    assert not decoder.needs_key_frame
    # Inter frames, not a stream of key frames (a backend may add a few).
    assert deltas >= 7, f"only {deltas} of 9 frames were inter frames"


@pytest.mark.parametrize("codec", CODECS, ids=lambda c: str(c).rsplit(".", 1)[-1])
def test_each_picture_is_an_array_of_its_own(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    # The backend writes each picture into a new array the caller owns: no
    # view of a buffer the decoder reuses, so a later decode cannot change it.
    _require(codecs, codec)
    encoder = codecs.encoder(codec, W, H, bitrate_bps=2_000_000, framerate=30.0)
    decoder = codecs.decoder(codec)
    pictures = []
    for i in range(3):
        encoded = encoder.encode(frame(i), i * FRAME_US)
        assert encoded is not None
        picture = decoder.decode(encoded.data)
        assert picture is not None
        assert picture.flags.c_contiguous and picture.flags.writeable
        pictures.append((picture, picture.copy()))
    for picture, snapshot in pictures:
        assert np.array_equal(picture, snapshot)
    for a, b in zip(pictures, pictures[1:]):
        assert not np.shares_memory(a[0], b[0])


@pytest.mark.parametrize("codec", CODECS, ids=lambda c: str(c).rsplit(".", 1)[-1])
def test_decode_does_not_trust_a_replaced_numpy_empty(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The picture's memory comes from the binding, not from anything Python
    # code can swap: an allocator that hands back one byte changes nothing.
    _require(codecs, codec)
    encoder = codecs.encoder(codec, W, H, bitrate_bps=2_000_000, framerate=30.0)
    decoder = codecs.decoder(codec)
    encoded = encoder.encode(frame(0), 0)
    assert encoded is not None
    monkeypatch.setattr(np, "empty", lambda *args, **kwargs: np.zeros(1, np.uint8))
    picture = decoder.decode(encoded.data)
    monkeypatch.undo()
    assert picture is not None
    assert picture.shape == (H, W, 3)
    assert psnr(picture, frame(0)) >= MIN_PSNR_DB


@pytest.mark.parametrize("codec", CODECS, ids=lambda c: str(c).rsplit(".", 1)[-1])
def test_key_frame_on_demand(codecs: rw.VideoCodecs, codec: rw.VideoCodec) -> None:
    _require(codecs, codec)
    encoder = codecs.encoder(codec, W, H)
    decoder = codecs.decoder(codec)
    stream = []
    for i in range(8):
        if i == 5:
            encoder.request_key_frame()
            # Asking twice before the frame is encoded changes nothing.
            encoder.request_key_frame()
        encoded = encoder.encode(frame(i), i * FRAME_US)
        assert encoded is not None
        stream.append(encoded)
        assert decoder.decode(encoded.data) is not None
    assert stream[0].key_frame
    assert stream[5].key_frame, "no key frame after request_key_frame()"
    assert not stream[6].key_frame, "the frame after the requested key frame is one too"
    # The marked key frame is one a fresh decoder starts from.
    fresh = codecs.decoder(codec)
    picture = fresh.decode(stream[5].data)
    assert picture is not None and psnr(frame(5), picture) >= MIN_PSNR_DB


@pytest.mark.parametrize("codec", CODECS, ids=lambda c: str(c).rsplit(".", 1)[-1])
def test_reset_decoder_waits_for_a_key_frame(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    _require(codecs, codec)
    encoder = codecs.encoder(codec, W, H)
    stream = [encoder.encode(frame(i), i * FRAME_US) for i in range(3)]
    assert all(f is not None for f in stream)

    decoder = codecs.decoder(codec)
    # A new decoder refuses an inter frame and keeps waiting.
    with pytest.raises(rw.CodecNeedsKeyFrame):
        decoder.decode(stream[1].data)
    assert decoder.needs_key_frame
    for f in stream:
        assert decoder.decode(f.data) is not None

    decoder.reset()
    assert decoder.needs_key_frame
    with pytest.raises(rw.CodecNeedsKeyFrame):
        decoder.decode(encoder.encode(frame(3), 3 * FRAME_US).data)

    encoder.request_key_frame()
    key = encoder.encode(frame(4), 4 * FRAME_US)
    assert key is not None and key.key_frame
    picture = decoder.decode(key.data)
    assert picture is not None and psnr(frame(4), picture) >= MIN_PSNR_DB
    assert not decoder.needs_key_frame


def test_encode_releases_the_gil(codecs: rw.VideoCodecs) -> None:
    """Two encoders on two threads both make progress, and a Python thread
    keeps running while they encode."""
    _require(codecs, rw.VideoCodec.Vp9)
    errors: list[BaseException] = []
    ticks = 0
    done = threading.Event()

    def ticker() -> None:
        nonlocal ticks
        while not done.is_set():
            ticks += 1

    def work() -> None:
        try:
            encoder = codecs.encoder(rw.VideoCodec.Vp9, W, H)
            decoder = codecs.decoder(rw.VideoCodec.Vp9)
            for i in range(10):
                encoded = encoder.encode(frame(i), i * FRAME_US)
                assert encoded is not None
                assert decoder.decode(encoded.data) is not None
        except BaseException as e:  # noqa: BLE001 - reported below
            errors.append(e)

    tick = threading.Thread(target=ticker)
    workers = [threading.Thread(target=work) for _ in range(2)]
    tick.start()
    for t in workers:
        t.start()
    for t in workers:
        t.join(timeout=60)
    done.set()
    tick.join()
    assert not errors, errors
    assert ticks > 0
