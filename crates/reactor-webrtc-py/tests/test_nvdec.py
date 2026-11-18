"""H.264 decode on NVIDIA GPUs with NVDEC.

Asking for NVDEC never fails: where no NVIDIA GPU decodes H.264 (any machine
without one, macOS) the backend is left out and H.264 decodes as before. The
tests that decode on NVDEC need such a GPU and are skipped elsewhere; the H.264
they decode comes from the process's own encoder (OpenH264 on Linux).
"""

from __future__ import annotations

import asyncio

import numpy as np
import pytest
import reactor_webrtc as rw
from test_loopback import connect, make_peer
from test_video_codec import FRAME_US, MIN_PSNR_DB, frame, psnr

W, H = 320, 180


@pytest.fixture(scope="module")
def codecs(openh264_path: str | None) -> rw.VideoCodecs:
    if openh264_path is not None:
        return rw.VideoCodecs.with_openh264(openh264_path, nvdec=True)
    return rw.VideoCodecs(nvdec=True)


def nvdec_decoders(codecs: rw.VideoCodecs) -> list[rw.CodecSupport]:
    return [s for s in codecs.capabilities().decoders if s.implementation == "NVDEC"]


@pytest.fixture(scope="module")
def nvdec(codecs: rw.VideoCodecs) -> rw.VideoCodecs:
    if not nvdec_decoders(codecs):
        pytest.skip("no NVIDIA GPU decodes H.264 here")
    if not codecs.capabilities().can_encode(rw.VideoCodec.H264):
        pytest.skip("no H.264 encoder to feed NVDEC")
    return codecs


def test_asking_for_nvdec_never_fails(codecs: rw.VideoCodecs) -> None:
    # Present or not, NVDEC is only ever an H.264 hardware decoder.
    for support in nvdec_decoders(codecs):
        assert support.codec == rw.VideoCodec.H264
        assert support.hardware
    assert not [s for s in codecs.capabilities().encoders if s.implementation == "NVDEC"]


def test_without_asking_there_is_no_nvdec() -> None:
    assert not nvdec_decoders(rw.VideoCodecs())


def test_nvdec_is_the_first_h264_decoder(nvdec: rw.VideoCodecs) -> None:
    decoder = nvdec.decoder(rw.VideoCodec.H264)

    assert decoder.implementation == "NVDEC"
    assert decoder.is_hardware


def test_a_stream_decodes_frame_by_frame(nvdec: rw.VideoCodecs) -> None:
    encoder = nvdec.encoder(rw.VideoCodec.H264, W, H, bitrate_bps=2_000_000, framerate=30.0)
    decoder = nvdec.decoder(rw.VideoCodec.H264, backend=rw.CodecBackend.Hardware)
    assert decoder.implementation == "NVDEC"

    for i in range(30):
        src = frame(i)
        encoded = encoder.encode(src, i * FRAME_US)
        assert encoded is not None
        # One access unit in, its picture out of the same call.
        picture = decoder.decode(encoded.data)
        assert picture is not None, f"frame {i} gave no picture"
        assert picture.shape == (H, W, 3)
        assert psnr(src, picture) >= MIN_PSNR_DB, f"frame {i}: {psnr(src, picture):.1f} dB"
    assert not decoder.needs_key_frame


def test_a_fresh_decoder_starts_at_a_key_frame(nvdec: rw.VideoCodecs) -> None:
    encoder = nvdec.encoder(rw.VideoCodec.H264, W, H)
    stream = [encoder.encode(frame(i), i * FRAME_US) for i in range(6)]
    encoder.request_key_frame()
    stream.append(encoder.encode(frame(6), 6 * FRAME_US))
    assert all(e is not None for e in stream)
    assert stream[6].key_frame

    decoder = nvdec.decoder(rw.VideoCodec.H264, backend=rw.CodecBackend.Hardware)
    picture = decoder.decode(stream[6].data)

    assert picture is not None and psnr(frame(6), picture) >= MIN_PSNR_DB


def test_a_new_size_mid_stream_decodes_at_the_new_size(nvdec: rw.VideoCodecs) -> None:
    decoder = nvdec.decoder(rw.VideoCodec.H264, backend=rw.CodecBackend.Hardware)
    for width, height in ((W, H), (640, 360), (W, H)):
        encoder = nvdec.encoder(rw.VideoCodec.H264, width, height)
        src = np.zeros((height, width, 3), dtype=np.uint8)
        src[..., 0] = 200
        encoded = encoder.encode(src, 0)
        assert encoded is not None and encoded.key_frame

        picture = decoder.decode(encoded.data)

        assert picture is not None and picture.shape == (height, width, 3)



async def test_a_peer_connection_decodes_h264_on_nvdec(factory, nvdec: rw.VideoCodecs) -> None:
    # The session factory asks for NVDEC, as an application would: a received
    # H.264 stream decodes on it.
    received: list = []  # keeps the Track alive: dropping it removes the sink

    def on_track(kind, track):
        if kind == rw.MediaKind.Video:
            received.append(track)
            track.on_video_frame(lambda *_: None)

    sender = make_peer(factory)
    receiver = make_peer(factory, on_track=on_track)
    video = factory.create_video_track("nvdec-video")
    transceiver = sender.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
    await transceiver.set_codec_preferences([rw.VideoCodec.H264])
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

    assert inbound is not None and inbound.frames_decoded >= 30, "no H.264 frames decoded"
    assert inbound.decoder_implementation == "NVDEC", inbound.decoder_implementation
