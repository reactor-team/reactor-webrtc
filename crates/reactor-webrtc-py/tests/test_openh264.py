"""Cisco's OpenH264 in the wheel: downloaded, registered, and negotiated.

Every wheel is built with the `openh264` feature, so on a platform with no
H.264 of its own (Linux, Windows) a factory with OpenH264 registered offers
H.264 and the standalone codecs encode and decode it. Without it a Linux
process offers no H.264 at all, and a peer that offers only H.264 for a video
section gets that section rejected.

The session factory (conftest) is built with OpenH264 registered where the
platform needs it. The download is real, once per machine, then cached.
"""

from __future__ import annotations

import os
import sys

import pytest
import reactor_webrtc as rw
from test_loopback import make_peer

needs_download = pytest.mark.skipif(
    sys.platform == "darwin", reason="Cisco ships no OpenH264 for macOS; H.264 is VideoToolbox"
)


def test_the_attribution_is_cisco_s_notice() -> None:
    assert rw.OPENH264_ATTRIBUTION == "OpenH264 Video Codec provided by Cisco Systems, Inc."


@needs_download
def test_the_library_is_downloaded_once_and_then_read_from_the_cache(openh264_path: str) -> None:
    assert os.path.isfile(openh264_path)
    assert rw.ensure_openh264() == openh264_path


@needs_download
def test_a_cache_dir_holds_its_own_copy(tmp_path) -> None:
    path = rw.ensure_openh264(str(tmp_path))

    assert path.startswith(str(tmp_path))
    assert os.path.isfile(path)


@pytest.mark.skipif(sys.platform != "darwin", reason="only macOS has no Cisco binary")
def test_macos_has_no_cisco_binary_to_download() -> None:
    with pytest.raises(RuntimeError, match="OpenH264 is not available"):
        rw.ensure_openh264()


@pytest.mark.parametrize(
    "direction", [rw.TransceiverDirection.SendOnly, rw.TransceiverDirection.RecvOnly]
)
async def test_a_video_section_offers_h264(factory, direction) -> None:
    # On Linux this is what registering OpenH264 changes: without it the offer
    # lists no H.264, and an answerer offered only H.264 rejects the section.
    peer = make_peer(factory)
    peer.pc.add_transceiver(rw.MediaKind.Video, direction)

    offer = await peer.pc.create_offer()

    assert "H264/90000" in offer.sdp


async def test_a_section_offered_only_h264_is_accepted(factory) -> None:
    offerer = make_peer(factory)
    answerer = make_peer(factory)
    transceiver = offerer.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
    await transceiver.set_codec_preferences([rw.VideoCodec.H264])
    offer = await offerer.pc.create_offer()
    await offerer.pc.set_local_description(offer)

    await answerer.pc.set_remote_description(offer)
    answer = await answerer.pc.create_answer()

    video = next(line for line in answer.sdp.splitlines() if line.startswith("m=video"))
    assert not video.startswith("m=video 0 "), answer.sdp
    assert "H264/90000" in answer.sdp


@needs_download
def test_the_standalone_codecs_add_openh264(openh264_path: str) -> None:
    # test_video_codec runs H.264 through these codecs end to end.
    caps = rw.VideoCodecs.with_openh264(openh264_path).capabilities()

    assert caps.can_encode(rw.VideoCodec.H264)
    assert caps.can_decode(rw.VideoCodec.H264)
