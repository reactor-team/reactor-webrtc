"""Shared fixtures for the reactor_webrtc Python test suite."""

import pytest
import reactor_webrtc as rw


@pytest.fixture(scope="session")
def factory() -> rw.PeerConnectionFactory:
    """One PeerConnectionFactory for the entire test session.

    Session scope is intentional: libwebrtc starts a set of process-global
    threads (network, signalling, worker) on factory creation and joins them on
    destruction. Repeatedly creating and tearing down factories in the same
    process does not give those threads enough time to fully stop before the
    next factory starts, leading to a segfault on the Nth cycle. A single
    session-scoped factory avoids all repeated lifecycle churn.

    Consequence: no test may create its own PeerConnectionFactory (e.g. via
    PeerConnectionFactory.with_encoded_video_track) — doing so would result in
    two concurrent factory instances, which is equally unsafe.
    """
    return rw.PeerConnectionFactory()
