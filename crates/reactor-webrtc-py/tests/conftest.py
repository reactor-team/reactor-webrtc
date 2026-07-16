"""Shared fixtures for the reactor_webrtc Python test suite."""

import pytest
import reactor_webrtc as rw


@pytest.fixture
def factory() -> rw.PeerConnectionFactory:
    """A fresh PeerConnectionFactory for each test.

    Function scope is intentional: libwebrtc has process-global state (network
    thread, signalling thread, DTLS identity factory) that does not support
    more than one PeerConnectionFactory alive at a time. A module-scoped
    fixture would keep the factory alive across tests that create their own
    factory (e.g. with_encoded_video_track), causing a segfault.
    """
    return rw.PeerConnectionFactory()
