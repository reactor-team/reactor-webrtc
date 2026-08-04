"""Regression tests for REA-4875 / REA-4766: a live PeerConnection, Track, or
DataChannel must not crash the interpreter when the factory that (directly or
indirectly) produced it is torn down.

These run each repro as a subprocess and assert on its exit code rather than
testing in-process. The crash only manifests during actual interpreter
finalisation — an in-process test keeps the pytest process itself running
long after the object under test is dropped, so it can never observe a
teardown-time segfault. A subprocess that segfaults is killed by SIGSEGV;
Python reports that as a negative return code (-11), not 0.

Run with:

    pytest crates/reactor-webrtc-py/tests/test_factory_lifetime.py -v
"""

import subprocess
import sys

TIMEOUT = 20.0  # seconds — generous for slow CI machines


def _run(script: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        timeout=TIMEOUT,
    )


def test_peer_connection_survives_process_exit():
    """A live PeerConnectionFactory + PeerConnection at exit must not segfault.

    Neither is explicitly dropped — the process ends with both still
    referenced, the exact shape of REA-4766's and REA-4875's reports.
    """
    result = _run(
        "import reactor_webrtc as rw\n"
        "factory = rw.PeerConnectionFactory()\n"
        "pc = factory.create_peer_connection(rw.RtcConfiguration(), rw.PeerConnectionObserver())\n"
        "pc.create_offer()\n"
    )
    assert result.returncode == 0, result.stderr.decode()


def test_data_channel_survives_factory_and_connection_drop():
    """A detached DataChannel must not segfault even once its connection and
    the factory that ultimately owns its threads are both dropped first.
    """
    result = _run(
        "import reactor_webrtc as rw\n"
        "factory = rw.PeerConnectionFactory()\n"
        "pc = factory.create_peer_connection(rw.RtcConfiguration(), rw.PeerConnectionObserver())\n"
        "dc = pc.create_data_channel('probe')\n"
        "del pc\n"
        "del factory\n"
        "dc.label()\n"
    )
    assert result.returncode == 0, result.stderr.decode()
