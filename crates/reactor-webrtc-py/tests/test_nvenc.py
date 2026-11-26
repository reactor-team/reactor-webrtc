"""Video encode on NVIDIA GPUs with NVENC: H.264 and AV1.

Asking for NVENC never fails: where no NVIDIA GPU with an encoder is (any
machine without one, an A100, H100 or B200, macOS) the backend is left out and
video encodes as before, and a codec the GPU does not encode stays in
software. The tests that encode on NVENC need a GPU that encodes the codec and
are skipped elsewhere; the streams they encode are decoded by the process's own
decoders (NVDEC where the GPU has it), so a codec this process cannot decode is
skipped.
"""

from __future__ import annotations

import asyncio

import numpy as np
import pytest
import reactor_webrtc as rw
from test_loopback import connect, make_peer
from test_video_codec import FRAME_US, MIN_PSNR_DB, frame, psnr

W, H = 320, 180
CODECS = [rw.VideoCodec.H264, rw.VideoCodec.Av1]


def ids(codec: rw.VideoCodec) -> str:
    return str(codec).rsplit(".", 1)[-1]


@pytest.fixture(scope="module")
def codecs() -> rw.VideoCodecs:
    return rw.VideoCodecs(nvdec=True, nvenc=True)


def nvenc_encoders(codecs: rw.VideoCodecs) -> list[rw.CodecSupport]:
    return [s for s in codecs.capabilities().encoders if s.implementation == "NVENC"]


def require_nvenc(codecs: rw.VideoCodecs, codec: rw.VideoCodec) -> None:
    if not any(s.codec == codec for s in nvenc_encoders(codecs)):
        pytest.skip(f"no NVIDIA GPU encodes {ids(codec)} here")
    if not codecs.capabilities().can_decode(codec):
        pytest.skip(f"no {ids(codec)} decoder to check NVENC's stream")


def test_asking_for_nvenc_never_fails(codecs: rw.VideoCodecs) -> None:
    # Present or not, NVENC is only ever a hardware encoder, of these codecs.
    for support in nvenc_encoders(codecs):
        assert support.codec in CODECS
        assert support.hardware
    assert not [
        s for s in codecs.capabilities().decoders if s.implementation == "NVENC"
    ]


def test_without_asking_there_is_no_nvenc() -> None:
    assert not nvenc_encoders(rw.VideoCodecs())
    assert not nvenc_encoders(rw.VideoCodecs(nvdec=True))


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_nvenc_is_the_first_encoder(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    require_nvenc(codecs, codec)
    encoder = codecs.encoder(codec, W, H)

    assert encoder.implementation == "NVENC"
    assert encoder.is_hardware


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_a_stream_encodes_frame_by_frame(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    require_nvenc(codecs, codec)
    encoder = codecs.encoder(
        codec,
        W,
        H,
        bitrate_bps=2_000_000,
        framerate=30.0,
        backend=rw.CodecBackend.Hardware,
    )
    decoder = codecs.decoder(codec)
    assert encoder.implementation == "NVENC"

    for i in range(30):
        src = frame(i)
        # One frame in, its encoded frame out of the same call: no B-frames,
        # no look-ahead.
        encoded = encoder.encode(src, i * FRAME_US)
        assert encoded is not None, f"frame {i} gave no encoded frame"
        assert encoded.key_frame == (i == 0), (
            f"frame {i}: key_frame={encoded.key_frame}"
        )
        picture = decoder.decode(encoded.data)
        assert picture is not None, f"frame {i} gave no picture"
        assert picture.shape == (H, W, 3)
        assert psnr(src, picture) >= MIN_PSNR_DB, (
            f"frame {i}: {psnr(src, picture):.1f} dB"
        )


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_a_key_frame_on_demand_starts_a_fresh_decoder(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    require_nvenc(codecs, codec)
    encoder = codecs.encoder(codec, W, H, backend=rw.CodecBackend.Hardware)
    stream = [encoder.encode(frame(i), i * FRAME_US) for i in range(6)]
    encoder.request_key_frame()
    stream.append(encoder.encode(frame(6), 6 * FRAME_US))
    assert all(e is not None for e in stream)
    # An infinite GOP: no key frame but the first, until one is asked for.
    assert [e.key_frame for e in stream] == [
        True,
        False,
        False,
        False,
        False,
        False,
        True,
    ]

    # The key frame carries its sequence header: a decoder that saw nothing
    # before starts from it.
    decoder = codecs.decoder(codec)
    picture = decoder.decode(stream[6].data)

    assert picture is not None and psnr(frame(6), picture) >= MIN_PSNR_DB


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_a_new_size_mid_stream_starts_at_a_key_frame(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    require_nvenc(codecs, codec)
    encoder = codecs.encoder(codec, W, H, backend=rw.CodecBackend.Hardware)
    decoder = codecs.decoder(codec)
    for i, (width, height) in enumerate(((W, H), (640, 360), (W, H))):
        src = np.zeros((height, width, 3), dtype=np.uint8)
        src[..., 0] = 200
        encoded = encoder.encode(src, i * FRAME_US)
        assert encoded is not None and encoded.key_frame
        assert (encoded.width, encoded.height) == (width, height)

        picture = decoder.decode(encoded.data)

        assert picture is not None and picture.shape == (height, width, 3)


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_the_stream_holds_the_bitrate(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    # CBR with a one-frame rate buffer: over two seconds of a moving pattern
    # the stream stays near the rate asked for, key frame included.
    require_nvenc(codecs, codec)
    bitrate = 500_000
    encoder = codecs.encoder(
        codec,
        640,
        360,
        bitrate_bps=bitrate,
        framerate=30.0,
        backend=rw.CodecBackend.Hardware,
    )
    total = 0
    for i in range(60):
        src = np.ascontiguousarray(np.tile(frame(i), (2, 2, 1)))
        encoded = encoder.encode(src, i * FRAME_US)
        assert encoded is not None
        total += len(encoded.data)

    rate = total * 8 / 2.0
    assert 0.5 * bitrate <= rate <= 1.5 * bitrate, f"{rate:.0f} bps for {bitrate} asked"


@pytest.mark.parametrize("codec", CODECS, ids=ids)
async def test_a_peer_connection_encodes_on_nvenc(
    factory, codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    # The session factory asks for NVENC, as an application would: a video
    # stream sent in a codec the GPU encodes encodes on it, and the peer
    # decodes it.
    require_nvenc(codecs, codec)
    received: list = []  # keeps the Track alive: dropping it removes the sink

    def on_track(kind, track):
        if kind == rw.MediaKind.Video:
            received.append(track)
            track.on_video_frame(lambda *_: None)

    sender = make_peer(factory)
    receiver = make_peer(factory, on_track=on_track)
    video = factory.create_video_track(f"nvenc-{ids(codec)}")
    transceiver = sender.pc.add_transceiver(
        rw.MediaKind.Video, rw.TransceiverDirection.SendOnly
    )
    await transceiver.set_codec_preferences([codec])
    await transceiver.set_track(video)
    assert await connect(sender, receiver), "peers did not connect within timeout"

    inbound = outbound = None
    for i in range(300):
        video.push_video_frame(bytes([(i * 7) % 256]) * (320 * 240 * 4), 320, 240)
        await asyncio.sleep(0.033)
        if i % 15 == 0:
            stats = await receiver.pc.get_stats()
            inbound = next(
                (s for s in stats.inbound_rtp if s.kind == rw.StreamKind.Video), None
            )
            if inbound is not None and inbound.frames_decoded >= 30:
                break

    stats = await sender.pc.get_stats()
    outbound = next(
        (s for s in stats.outbound_rtp if s.kind == rw.StreamKind.Video), None
    )
    assert inbound is not None and inbound.frames_decoded >= 30, (
        "too few frames decoded"
    )
    assert outbound is not None and outbound.encoder_implementation == "NVENC", (
        outbound and outbound.encoder_implementation
    )


async def test_a_peer_connection_sends_the_colour_nvenc_encoded(
    factory, codecs: rw.VideoCodecs
) -> None:
    # A stream NVENC encodes reaches the peer's sink as the colour sent.
    codec = rw.VideoCodec.Av1
    require_nvenc(codecs, codec)
    received: list = []  # keeps the Track alive: dropping it removes the sink
    pixels: list[tuple[int, int, int]] = []

    def on_frame(data, width, height, *_):
        i = ((height // 2) * width + width // 2) * 3
        pixels.append((data[i], data[i + 1], data[i + 2]))

    def on_track(kind, track):
        if kind == rw.MediaKind.Video:
            received.append(track)
            track.on_video_frame(on_frame, format="rgb24")

    sender = make_peer(factory)
    receiver = make_peer(factory, on_track=on_track)
    video = factory.create_video_track("nvenc-colour")
    transceiver = sender.pc.add_transceiver(
        rw.MediaKind.Video, rw.TransceiverDirection.SendOnly
    )
    await transceiver.set_codec_preferences([codec])
    await transceiver.set_track(video)
    assert await connect(sender, receiver), "peers did not connect within timeout"

    red_bgra = bytes([0, 0, 255, 255]) * (320 * 240)
    for _ in range(300):
        video.push_video_frame(red_bgra, 320, 240)
        await asyncio.sleep(0.033)
        if len(pixels) >= 30:
            break

    stats = await sender.pc.get_stats()
    outbound = next(
        (s for s in stats.outbound_rtp if s.kind == rw.StreamKind.Video), None
    )
    assert len(pixels) >= 30, "too few frames reached the sink"
    assert outbound is not None and outbound.encoder_implementation == "NVENC"
    r, g, b = pixels[-1]
    # Through an encoder and back, red stays far from the other two channels.
    assert r > 200 and g < 60 and b < 60, (r, g, b)
