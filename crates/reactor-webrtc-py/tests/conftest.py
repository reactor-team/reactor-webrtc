"""Shared fixtures for the reactor_webrtc Python test suite."""

import pytest
import reactor_webrtc as rw


@pytest.fixture(scope="module")
def factory() -> rw.PeerConnectionFactory:
    """A single PeerConnectionFactory reused across all tests in a module."""
    return rw.PeerConnectionFactory()
