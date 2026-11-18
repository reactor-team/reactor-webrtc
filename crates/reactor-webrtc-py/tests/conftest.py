"""Shared fixtures for the reactor_webrtc Python test suite."""

import sys

import pytest
import reactor_webrtc as rw


@pytest.fixture(scope="session")
def openh264_path() -> str | None:
    """Cisco's OpenH264, downloaded once per machine, where the platform has no
    H.264 of its own; ``None`` on macOS, where H.264 is VideoToolbox.

    A download that fails fails the tests that use it rather than skipping
    them: a wheel that cannot reach H.264 on Linux is the bug they catch.
    """
    if sys.platform == "darwin":
        return None
    return rw.ensure_openh264()


@pytest.fixture(scope="session")
def factory(openh264_path: str | None) -> rw.PeerConnectionFactory:
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
    builder = rw.PeerConnectionFactoryBuilder()
    # Built the way an application ships it: with OpenH264 registered where the
    # platform needs it, so H.264 is negotiated and encoded on every platform.
    if openh264_path is not None:
        builder.with_openh264(openh264_path)
    return builder.build()
