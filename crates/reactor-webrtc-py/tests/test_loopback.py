"""Loopback integration test: two in-process PeerConnections over data channel.

Run after `maturin develop`:

    pytest crates/reactor-webrtc-py/tests/test_loopback.py -v

Requires a built reactor_webrtc extension (REACTOR_WEBRTC_PREBUILT_URL must
have been set at maturin build time).
"""

import threading
import time

import pytest
import reactor_webrtc as rw


def negotiate(pc1: rw.PeerConnection, pc2: rw.PeerConnection) -> None:
    offer = pc1.create_offer()
    pc1.set_local_description(offer)
    pc2.set_remote_description(offer)
    answer = pc2.create_answer()
    pc2.set_local_description(answer)
    pc1.set_remote_description(answer)


def wait_for(condition, timeout: float = 20.0, poll: float = 0.025) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return True
        time.sleep(poll)
    return False


def make_peer(factory: rw.PeerConnectionFactory):
    ice_queue: list[rw.IceCandidate] = []
    connected = threading.Event()
    lock = threading.Lock()

    obs = rw.PeerConnectionObserver()
    obs.on_ice_candidate = lambda c: ice_queue.append(c)
    obs.on_connection_state_change = (
        lambda s: connected.set() if s == rw.PeerConnectionState.Connected else None
    )

    pc = factory.create_peer_connection(rw.RtcConfiguration(), obs)
    return pc, ice_queue, connected, lock


def trickle(from_queue: list, to_pc: rw.PeerConnection) -> None:
    for cand in list(from_queue):
        to_pc.add_ice_candidate(cand)
    from_queue.clear()


class TestLoopback:
    def test_data_channel_send_receive(self):
        factory = rw.PeerConnectionFactory()

        pc1, ice1, connected1, _ = make_peer(factory)
        pc2, ice2, connected2, _ = make_peer(factory)

        received: list[bytes] = []
        dc2_holder: list[rw.DataChannel] = []

        obs2 = rw.PeerConnectionObserver()
        obs2.on_data_channel = lambda dc: (
            dc2_holder.append(dc),
            dc.on_message(lambda data, binary: received.append(data)),
        )

        # Re-create pc2 with the data-channel observer
        pc2, ice2, connected2, _ = make_peer(factory)
        # Patch the observer in — we have to rebuild since create_peer_connection
        # consumes the observer. Use a fresh pair instead.
        pc1, ice1, connected1, _ = make_peer(factory)
        pc2_obs = rw.PeerConnectionObserver()
        pc2_obs.on_data_channel = lambda dc: (
            dc2_holder.append(dc),
            dc.on_message(lambda data, _binary: received.append(data)),
        )
        pc2 = factory.create_peer_connection(rw.RtcConfiguration(), pc2_obs)

        dc1 = pc1.create_data_channel("test")
        dc1_open = threading.Event()
        dc1.on_open(dc1_open.set)

        negotiate(pc1, pc2)

        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            trickle(ice1, pc2)
            trickle(ice2, pc1)
            if dc1_open.is_set():
                break
            time.sleep(0.025)

        assert dc1_open.is_set(), "data channel did not open within 20s"

        dc1.send(b"hello", True)
        dc1.send(b"world", False)

        ok = wait_for(lambda: len(received) >= 2)
        assert ok, "messages not received within 20s"
        assert set(received) == {b"hello", b"world"}

    def test_factory_creates_tracks(self):
        factory = rw.PeerConnectionFactory()
        video = factory.create_video_track("v1")
        audio = factory.create_audio_track("a1")
        assert video.kind() == rw.MediaKind.Video
        assert audio.kind() == rw.MediaKind.Audio

    def test_encoded_video_track(self):
        factory, evtrack = rw.PeerConnectionFactory.with_encoded_video_track(
            "cam", 640, 480
        )
        track = evtrack.track()
        assert track.kind() == rw.MediaKind.Video

    def test_ice_server_config(self):
        server = rw.IceServer(
            urls=["stun:stun.l.google.com:19302"],
            username="",
            password="",
        )
        config = rw.RtcConfiguration(ice_servers=[server])
        assert len(config.ice_servers) == 1
        assert config.ice_servers[0].urls == ["stun:stun.l.google.com:19302"]
