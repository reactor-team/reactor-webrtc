"""H.265 over a peer connection: RTP packetization (RFC 7798) both ways.

libwebrtc is built with `rtc_use_h265=true`, which brings H.265's packetizer,
depacketizer and SDP negotiation but no H.265 codec. A pre-encoded track sends
an H.265 stream (`data/h265_320x240.hevc`: 320x240 Main, a key frame with its
VPS, SPS and PPS, then three P frames, from libx265) and the receiver's frame
transform sees each frame as it comes out of the depacketizer, before the
decoder. Every NAL unit sent arrives unchanged, the key frame (about 10 kB)
split across packets and put back together.

Must be run as a separate pytest invocation: `with_encoded_video_track`
creates the process's one PeerConnectionFactory.
"""

from __future__ import annotations

import asyncio
import re
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

import pytest
import reactor_webrtc as rw

TIMEOUT = 20.0
POLL = 0.025
STREAM = Path(__file__).parent / "data" / "h265_320x240.hevc"


def nal_units(annex_b: bytes) -> list[bytes]:
    """The NAL units of an Annex B stream, without their start codes."""
    starts = [m.end() for m in re.finditer(b"\x00\x00\x01", annex_b)]
    units = []
    for i, start in enumerate(starts):
        end = starts[i + 1] - 3 if i + 1 < len(starts) else len(annex_b)
        # A four-byte start code leaves its leading zero on the unit before.
        units.append(annex_b[start:end].rstrip(b"\x00"))
    return units


def access_units(annex_b: bytes) -> list[list[bytes]]:
    """The stream's NAL units grouped into frames: a frame starts at its
    parameter sets or at a slice whose first_slice_segment_in_pic_flag is set."""
    frames: list[list[bytes]] = []
    current: list[bytes] = []
    for unit in nal_units(annex_b):
        nal_type = (unit[0] >> 1) & 0x3F
        is_vcl = nal_type < 32
        first_slice = is_vcl and bool(unit[2] & 0x80)
        starts_frame = nal_type in (32, 33, 34) or first_slice
        if starts_frame and any(((u[0] >> 1) & 0x3F) < 32 for u in current):
            frames.append(current)
            current = []
        current.append(unit)
    if current:
        frames.append(current)
    return frames


def annex_b(units: list[bytes]) -> bytes:
    return b"".join(b"\x00\x00\x00\x01" + u for u in units)


@dataclass
class Peer:
    pc: rw.PeerConnection
    ice: list = field(default_factory=list)
    connected: threading.Event = field(default_factory=threading.Event)


def make_peer(factory: rw.PeerConnectionFactory, *, on_track=None) -> Peer:
    peer = Peer(pc=None)  # type: ignore[arg-type]
    obs = rw.PeerConnectionObserver()
    obs.on_ice_candidate = lambda c: peer.ice.append(c)
    obs.on_connection_state_change = lambda s: (
        peer.connected.set() if s == rw.PeerConnectionState.Connected else None
    )
    if on_track is not None:
        obs.on_track = on_track
    peer.pc = factory.create_peer_connection(rw.RtcConfiguration(), obs)
    return peer


async def connect(p1: Peer, p2: Peer) -> bool:
    offer = await p1.pc.create_offer()
    await p1.pc.set_local_description(offer)
    await p2.pc.set_remote_description(offer)
    answer = await p2.pc.create_answer()
    await p2.pc.set_local_description(answer)
    await p1.pc.set_remote_description(answer)
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        for c in list(p1.ice):
            await p2.pc.add_ice_candidate(c)
        p1.ice.clear()
        for c in list(p2.ice):
            await p1.pc.add_ice_candidate(c)
        p2.ice.clear()
        if p1.connected.is_set() and p2.connected.is_set():
            return True
        await asyncio.sleep(POLL)
    return False


@pytest.fixture(scope="session")
def factory_and_track():
    return rw.PeerConnectionFactory.with_encoded_video_track("h265", 320, 240)


def test_the_fixture_is_a_key_frame_and_three_p_frames() -> None:
    frames = access_units(STREAM.read_bytes())
    assert [[(u[0] >> 1) & 0x3F for u in f] for f in frames] == [
        [32, 33, 34, 20],  # VPS, SPS, PPS, IDR_N_LP
        [1],
        [1],
        [1],
    ]
    # The key frame is far larger than one packet.
    assert len(annex_b(frames[0])) > 5 * 1200


async def test_h265_frames_cross_a_peer_connection_unchanged(factory_and_track) -> None:
    factory, track = factory_and_track
    frames = access_units(STREAM.read_bytes())
    received: list[list[bytes]] = []
    keep: list = []  # the remote Track and the FrameTransform stay alive

    def on_received(frame):
        received.append(nal_units(bytes(frame.data)))
        return rw.FrameAction.Forward

    def on_track(kind, remote):
        if kind == rw.MediaKind.Video:
            keep.append(remote)

    sender = make_peer(factory)
    receiver = make_peer(factory, on_track=on_track)
    transceiver = sender.pc.add_transceiver(
        rw.MediaKind.Video, rw.TransceiverDirection.SendOnly
    )
    await transceiver.set_codec_preferences([rw.VideoCodec.H265])
    await transceiver.set_track(track)
    assert await connect(sender, receiver), "peers did not connect within timeout"

    transform = rw.FrameTransform(on_received)
    keep.append(transform)
    for t in await receiver.pc.transceivers():
        if t.kind() == rw.MediaKind.Video:
            t.set_receiver_transform(transform)
            break

    # The stream over and over, from its key frame, until each of its frames
    # has arrived at least once.
    deadline = time.monotonic() + TIMEOUT
    i = 0
    while time.monotonic() < deadline and not all(f in received for f in frames):
        frame = frames[i % len(frames)]
        track.push_encoded_frame(annex_b(frame), is_key_frame=(i % len(frames) == 0))
        i += 1
        await asyncio.sleep(0.033)

    # The codec first: another codec's packetizer would carry these bytes
    # through unchanged too, so the frames prove nothing unless it is H.265.
    stats = await receiver.pc.get_stats()
    inbound = next(s for s in stats.inbound_rtp if s.kind == rw.StreamKind.Video)
    assert inbound.codec_mime_type == "video/H265", inbound.codec_mime_type
    missing = [n for n, f in enumerate(frames) if f not in received]
    assert not missing, (
        f"frames {missing} never arrived unchanged; received NAL types: "
        f"{[[(u[0] >> 1) & 0x3F for u in r] for r in received[:8]]}"
    )
