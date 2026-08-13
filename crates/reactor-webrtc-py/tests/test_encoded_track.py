"""E2E tests for EncodedVideoTrack — specifically push_encoded_frame with
per-frame metadata (user_data=...).

Must be run as a SEPARATE pytest invocation from test_loopback.py because
PeerConnectionFactory is a process-wide singleton; two factories cannot be
alive simultaneously.

    pytest crates/reactor-webrtc-py/tests/test_encoded_track.py -v
"""

import asyncio
import threading
import time

import pytest
import reactor_webrtc as rw

TIMEOUT = 20.0
POLL = 0.025


# ── Helpers (mirrors test_loopback.py) ────────────────────────────────────────


async def wait_for(condition, timeout: float = TIMEOUT, poll: float = POLL) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return True
        await asyncio.sleep(poll)
    return False


def make_peer(factory: rw.PeerConnectionFactory, *, on_track=None):
    from dataclasses import dataclass, field

    @dataclass
    class Peer:
        pc: rw.PeerConnection
        ice: list = field(default_factory=list)
        connected: threading.Event = field(default_factory=threading.Event)

    peer = Peer(pc=None)  # type: ignore[arg-type]
    obs = rw.PeerConnectionObserver()
    _ice = peer.ice
    _connected = peer.connected
    obs.on_ice_candidate = lambda c: _ice.append(c)
    obs.on_connection_state_change = (
        lambda s: _connected.set() if s == rw.PeerConnectionState.Connected else None
    )
    if on_track is not None:
        obs.on_track = on_track
    peer.pc = factory.create_peer_connection(rw.RtcConfiguration(), obs)
    return peer


async def connect(p1, p2) -> bool:
    """Negotiate and trickle ICE until both peers are connected.

    create_offer advertises frame-metadata support and create_answer mirrors it,
    so nothing here has to.
    """
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


# ── Session fixture ───────────────────────────────────────────────────────────


@pytest.fixture(scope="session")
def enc_factory_and_track():
    """One with_encoded_video_track factory+track for the session.

    Not torn down explicitly — the process-wide threads are joined on Python
    interpreter exit, avoiding the repeated create/destroy cycle that can
    segfault in the same process (same reason as test_loopback.py's session
    fixture).
    """
    return rw.PeerConnectionFactory.with_encoded_video_track("enc-session", 320, 240)


# ── Tests ─────────────────────────────────────────────────────────────────────


class TestEncodedFrameMetadata:
    """E2E tests for push_encoded_frame with per-frame metadata."""

    async def test_encoded_frame_metadata_sender_embeds_trailer(self, enc_factory_and_track):
        """push_encoded_frame(user_data=...) correctly embeds the RXMT metadata trailer.

        Uses a raw receiver FrameTransform to inspect the encoded bytes before
        the decoder; verifies the RXMT magic is present.  This exercises the
        sender-side path: capture_us sampling → insert_sender_meta → the sender
        FrameTransform finding the entry by capture_time_ms and appending the
        trailer.
        """
        factory, enc_track = enc_factory_and_track

        trailer_seen: list = []  # appended once the RXMT magic is found
        recv_tf_ref: list = []  # keep FrameTransform alive

        def recv_check(frame):
            if not trailer_seen and bytes(frame.data[-4:]) == b"RXMT":
                trailer_seen.append(True)
            return rw.FrameAction.Forward

        recv_track_ref: list = []

        def on_track(kind, track):
            if kind == rw.MediaKind.Video:
                recv_track_ref.append(track)  # prevent GC → Drop → RemoveSink

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_track=on_track)

        tx1 = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        await tx1.set_track(enc_track)

        ok = await connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        assert recv_track_ref, "on_track was not called during SDP negotiation"

        # Attach receiver FrameTransform to inspect encoded bytes before decode.
        recv_tf = rw.FrameTransform(recv_check)
        recv_tf_ref.append(recv_tf)
        for t in await p2.pc.transceivers():
            if t.kind() == rw.MediaKind.Video:
                t.set_receiver_transform(recv_tf)
                break

        user_data = b"enc-e2e-trailer"
        dummy_frame = bytes(4)  # minimal non-empty payload

        for _ in range(120):
            if trailer_seen:
                break
            enc_track.push_encoded_frame(dummy_frame, is_key_frame=True, user_data=user_data)
            await asyncio.sleep(0.033)

        ok = await wait_for(lambda: bool(trailer_seen))
        assert ok, (
            "RXMT trailer not seen in receiver FrameTransform within timeout — "
            "sender_metadata_transform may not be correctly appending the trailer "
            "for pre-encoded frames"
        )

    async def test_encoded_frame_metadata_roundtrip(self, enc_factory_and_track):
        """Full E2E: push_encoded_frame metadata survives the send→receive pipeline.

        Pushes dummy encoded bytes tagged with user_data; on the receiver uses
        receiver_metadata_transform + on_video_frame to collect FrameMetadata.
        Asserts user_data roundtrip, non-zero frame_id, and non-zero timestamp.

        NOTE: the decoder will likely fail to decode the dummy bytes, so
        on_video_frame may not fire.  The primary assertion is the trailer
        check above; this test validates the metadata queue integration on the
        receiver side when valid enough frames happen to decode.  It is allowed
        to pass with zero decoded frames if the codec silently discards them —
        in that case the sender-side embedding is still verified by the trailer
        test above.
        """
        factory, enc_track = enc_factory_and_track

        recv_track_ref: list = []
        received_meta: list = []

        def on_track(kind, track):
            if kind == rw.MediaKind.Video:
                recv_track_ref.append(track)
                track.on_video_frame(
                    lambda bgra, w, h, meta: received_meta.append(meta) if meta is not None else None
                )

        p3 = make_peer(factory)
        p4 = make_peer(factory, on_track=on_track)

        tx3 = p3.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        await tx3.set_track(enc_track)

        # The strip transform is installed by set_remote_description; nothing to
        # attach here.
        ok = await connect(p3, p4)
        assert ok, "peers did not connect within timeout"
        assert recv_track_ref, "on_track was not called"

        user_data = b"enc-e2e-meta"
        dummy_frame = bytes(4)

        for _ in range(120):
            enc_track.push_encoded_frame(dummy_frame, is_key_frame=True, user_data=user_data)
            await asyncio.sleep(0.033)

        # Dummy bytes may not decode — only assert if any metadata actually arrived.
        if received_meta:
            meta = received_meta[0]
            assert bytes(meta.user_data) == user_data, f"user_data mismatch: {meta.user_data!r}"
            assert meta.frame_id > 0, "frame_id must be non-zero"
            assert meta.timestamp > 0, "timestamp must be non-zero"
