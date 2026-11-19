"""Video decode on NVIDIA GPUs with NVDEC: H.264, VP8, VP9 and AV1.

Asking for NVDEC never fails: where no NVIDIA GPU is (any machine without
one, macOS) the backend is left out and video decodes as before, and a codec
the GPU does not decode stays in software. The tests that decode on NVDEC need
a GPU that decodes the codec and are skipped elsewhere; the streams they decode
come from the process's own encoders, so a codec this process cannot encode
(H.264 on Linux without OpenH264) is skipped.
"""

from __future__ import annotations

import asyncio

import numpy as np
import pytest
import reactor_webrtc as rw
from test_loopback import connect, make_peer
from test_video_codec import FRAME_US, MIN_PSNR_DB, frame, psnr

W, H = 320, 180
CODECS = [rw.VideoCodec.H264, rw.VideoCodec.Vp8, rw.VideoCodec.Vp9, rw.VideoCodec.Av1]


def ids(codec: rw.VideoCodec) -> str:
    return str(codec).rsplit(".", 1)[-1]


@pytest.fixture(scope="module")
def codecs() -> rw.VideoCodecs:
    return rw.VideoCodecs(nvdec=True)


def nvdec_decoders(codecs: rw.VideoCodecs) -> list[rw.CodecSupport]:
    return [s for s in codecs.capabilities().decoders if s.implementation == "NVDEC"]


def require_nvdec(codecs: rw.VideoCodecs, codec: rw.VideoCodec) -> None:
    if not any(s.codec == codec for s in nvdec_decoders(codecs)):
        pytest.skip(f"no NVIDIA GPU decodes {ids(codec)} here")
    if not codecs.capabilities().can_encode(codec):
        pytest.skip(f"no {ids(codec)} encoder to feed NVDEC")


def test_asking_for_nvdec_never_fails(codecs: rw.VideoCodecs) -> None:
    # Present or not, NVDEC is only ever a hardware decoder, of these codecs.
    for support in nvdec_decoders(codecs):
        assert support.codec in CODECS
        assert support.hardware
    assert not [s for s in codecs.capabilities().encoders if s.implementation == "NVDEC"]


def test_without_asking_there_is_no_nvdec() -> None:
    assert not nvdec_decoders(rw.VideoCodecs())


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_nvdec_is_the_first_decoder(codecs: rw.VideoCodecs, codec: rw.VideoCodec) -> None:
    require_nvdec(codecs, codec)
    decoder = codecs.decoder(codec)

    assert decoder.implementation == "NVDEC"
    assert decoder.is_hardware


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_a_stream_decodes_frame_by_frame(codecs: rw.VideoCodecs, codec: rw.VideoCodec) -> None:
    require_nvdec(codecs, codec)
    encoder = codecs.encoder(codec, W, H, bitrate_bps=2_000_000, framerate=30.0)
    decoder = codecs.decoder(codec, backend=rw.CodecBackend.Hardware)
    assert decoder.implementation == "NVDEC"

    for i in range(30):
        src = frame(i)
        encoded = encoder.encode(src, i * FRAME_US)
        assert encoded is not None
        # One frame in, its picture out of the same call.
        picture = decoder.decode(encoded.data)
        assert picture is not None, f"frame {i} gave no picture"
        assert picture.shape == (H, W, 3)
        assert psnr(src, picture) >= MIN_PSNR_DB, f"frame {i}: {psnr(src, picture):.1f} dB"
    assert not decoder.needs_key_frame


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_a_fresh_decoder_starts_at_a_key_frame(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    require_nvdec(codecs, codec)
    encoder = codecs.encoder(codec, W, H)
    stream = [encoder.encode(frame(i), i * FRAME_US) for i in range(6)]
    encoder.request_key_frame()
    stream.append(encoder.encode(frame(6), 6 * FRAME_US))
    assert all(e is not None for e in stream)
    assert stream[6].key_frame

    decoder = codecs.decoder(codec, backend=rw.CodecBackend.Hardware)
    picture = decoder.decode(stream[6].data)

    assert picture is not None and psnr(frame(6), picture) >= MIN_PSNR_DB


@pytest.mark.parametrize("codec", CODECS, ids=ids)
def test_a_new_size_mid_stream_decodes_at_the_new_size(
    codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    require_nvdec(codecs, codec)
    decoder = codecs.decoder(codec, backend=rw.CodecBackend.Hardware)
    for width, height in ((W, H), (640, 360), (W, H)):
        encoder = codecs.encoder(codec, width, height)
        src = np.zeros((height, width, 3), dtype=np.uint8)
        src[..., 0] = 200
        encoded = encoder.encode(src, 0)
        assert encoded is not None and encoded.key_frame

        picture = decoder.decode(encoded.data)

        assert picture is not None and picture.shape == (height, width, 3)


@pytest.mark.parametrize("codec", CODECS, ids=ids)
async def test_a_peer_connection_decodes_on_nvdec(
    factory, codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    # The session factory asks for NVDEC, as an application would: a received
    # video stream in a codec the GPU decodes decodes on it.
    require_nvdec(codecs, codec)
    received: list = []  # keeps the Track alive: dropping it removes the sink

    def on_track(kind, track):
        if kind == rw.MediaKind.Video:
            received.append(track)
            track.on_video_frame(lambda *_: None)

    sender = make_peer(factory)
    receiver = make_peer(factory, on_track=on_track)
    video = factory.create_video_track(f"nvdec-{ids(codec)}")
    transceiver = sender.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
    await transceiver.set_codec_preferences([codec])
    await transceiver.set_track(video)
    assert await connect(sender, receiver), "peers did not connect within timeout"

    inbound = None
    for i in range(300):
        video.push_video_frame(bytes([(i * 7) % 256]) * (320 * 240 * 4), 320, 240)
        await asyncio.sleep(0.033)
        if i % 15 == 0:
            stats = await receiver.pc.get_stats()
            inbound = next((s for s in stats.inbound_rtp if s.kind == rw.StreamKind.Video), None)
            if inbound is not None and inbound.frames_decoded >= 30:
                break

    assert inbound is not None and inbound.frames_decoded >= 30, "too few frames decoded"
    assert inbound.decoder_implementation == "NVDEC", inbound.decoder_implementation


@pytest.mark.parametrize("codec", [rw.VideoCodec.Vp9, rw.VideoCodec.Av1], ids=ids)
async def test_a_stream_too_small_for_nvdec_decodes_in_software(
    factory, codecs: rw.VideoCodecs, codec: rw.VideoCodec
) -> None:
    # NVDEC decodes VP9 and AV1 from 128x128 up: a smaller stream, as
    # libwebrtc sends under low bandwidth, falls back to the software decoder
    # and keeps decoding.
    require_nvdec(codecs, codec)
    received: list = []  # keeps the Track alive: dropping it removes the sink

    def on_track(kind, track):
        if kind == rw.MediaKind.Video:
            received.append(track)
            track.on_video_frame(lambda *_: None)

    sender = make_peer(factory)
    receiver = make_peer(factory, on_track=on_track)
    video = factory.create_video_track(f"nvdec-small-{ids(codec)}")
    transceiver = sender.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
    await transceiver.set_codec_preferences([codec])
    await transceiver.set_track(video)
    assert await connect(sender, receiver), "peers did not connect within timeout"

    inbound = None
    for i in range(300):
        video.push_video_frame(bytes([(i * 7) % 256]) * (96 * 96 * 4), 96, 96)
        await asyncio.sleep(0.033)
        if i % 15 == 0:
            stats = await receiver.pc.get_stats()
            inbound = next((s for s in stats.inbound_rtp if s.kind == rw.StreamKind.Video), None)
            if inbound is not None and inbound.frames_decoded >= 30:
                break

    assert inbound is not None and inbound.frames_decoded >= 30, "too few frames decoded"
    assert "fallback from: NVDEC" in inbound.decoder_implementation, (
        inbound.decoder_implementation
    )
